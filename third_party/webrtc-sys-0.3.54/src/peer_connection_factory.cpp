/*
 * Copyright 2025 LiveKit, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "livekit/peer_connection_factory.h"

#include <algorithm>
#include <cmath>
#include <memory>
#include <mutex>
#include <optional>
#include <utility>
#include <vector>

#include "api/audio_codecs/builtin_audio_decoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/audio/builtin_audio_processing_builder.h"
#include "api/create_modular_peer_connection_factory.h"
#include "api/environment/environment_factory.h"
#include "api/field_trials_view.h"
#include "api/peer_connection_interface.h"
#include "api/rtc_error.h"
#include "api/enable_media.h"
#include "api/rtc_event_log/rtc_event_log_factory.h"
#include "api/task_queue/default_task_queue_factory.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/audio/audio_device.h"
#include "api/audio_options.h"
#include "livekit/adm_proxy.h"
#include "livekit/audio_track.h"
#include "livekit/peer_connection.h"
#include "livekit/rtc_error.h"
#include "livekit/rtp_parameters.h"
#include "livekit/video_decoder_factory.h"
#include "livekit/video_encoder_factory.h"
#include "livekit/webrtc.h"
#include "rtc_base/thread.h"
#include "webrtc-sys/src/peer_connection.rs.h"
#include "webrtc-sys/src/peer_connection_factory.rs.h"

namespace livekit_ffi {
namespace {

constexpr char kForcePlayoutDelayFieldTrial[] =
    "WebRTC-ForcePlayoutDelay/min_ms:0,max_ms:0/";
constexpr char kForcePlayoutDelayValue[] = "min_ms:0,max_ms:0";

// PERSEPHONE PATCH (ADR-0062): the live factories and the microphone gain
// that every one of them applies. A factory registers itself after it is
// built and leaves before it is torn down; the mutex also keeps a gain
// change from running on a factory that is going away.
struct CaptureGainRegistry {
  std::mutex mutex;
  std::vector<const PeerConnectionFactory*> factories;
  uint16_t gain_percent = 100;
};

CaptureGainRegistry& capture_gain_registry() {
  static CaptureGainRegistry registry;
  return registry;
}

constexpr uint16_t kMaxCaptureGainPercent = 400;

int8_t tri_state(const std::optional<bool>& value) {
  return value.has_value() ? (*value ? 1 : 0) : -1;
}

class ZeroPlayoutDelayFieldTrials final : public webrtc::FieldTrialsView {
 public:
  std::string Lookup(absl::string_view key) const override {
    return key == "WebRTC-ForcePlayoutDelay" ? kForcePlayoutDelayValue : "";
  }

  std::unique_ptr<webrtc::FieldTrialsView> CreateCopy() const override {
    return std::make_unique<ZeroPlayoutDelayFieldTrials>();
  }
};

// Enables SPED (DTLS-in-STUN) via the WebRTC-IceHandshakeDtls field trial.
// SNAP (SCTP-INIT-in-SDP) is intentionally NOT set here: it maps to the
// immutable enable_sctp_snap RTCConfiguration field, so it must be carried on
// the RtcConfiguration (see RtcConfiguration.enable_sctp_snap) to stay
// consistent across create + set_configuration. Enabling it via a field trial
// makes set_configuration fail ("Modifying the configuration in an unsupported
// way").
class EnableWarpFieldTrials final : public webrtc::FieldTrialsView {
 public:
  std::string Lookup(absl::string_view key) const override {
    if (key == "WebRTC-IceHandshakeDtls") {
      return "Enabled";
    }
    return "";
  }

  std::unique_ptr<webrtc::FieldTrialsView> CreateCopy() const override {
    return std::make_unique<EnableWarpFieldTrials>();
  }
};

// An Environment accepts a single FieldTrialsView, so to enable several
// independent trial groups (e.g. zero-playout-delay AND WARP) we combine their
// views into one: Lookup delegates to each sub-view and returns the first
// non-empty result. The groups' keys are disjoint, so ordering is irrelevant.
class CompositeFieldTrials final : public webrtc::FieldTrialsView {
 public:
  explicit CompositeFieldTrials(
      std::vector<std::unique_ptr<webrtc::FieldTrialsView>> views)
      : views_(std::move(views)) {}

  std::string Lookup(absl::string_view key) const override {
    for (const auto& view : views_) {
      std::string value = view->Lookup(key);
      if (!value.empty()) {
        return value;
      }
    }
    return "";
  }

  std::unique_ptr<webrtc::FieldTrialsView> CreateCopy() const override {
    std::vector<std::unique_ptr<webrtc::FieldTrialsView>> copies;
    copies.reserve(views_.size());
    for (const auto& view : views_) {
      copies.push_back(view->CreateCopy());
    }
    return std::make_unique<CompositeFieldTrials>(std::move(copies));
  }

 private:
  std::vector<std::unique_ptr<webrtc::FieldTrialsView>> views_;
};

// zero_playout_delay and enable_warp are independent and may both be enabled;
// their field-trial views are composed into one.
webrtc::Environment CreateEnvironment(bool zero_playout_delay,
                                      bool enable_warp) {
  std::vector<std::unique_ptr<webrtc::FieldTrialsView>> views;
  if (zero_playout_delay) {
    views.push_back(std::make_unique<ZeroPlayoutDelayFieldTrials>());
  }
  if (enable_warp) {
    views.push_back(std::make_unique<EnableWarpFieldTrials>());
  }

  if (views.empty()) {
    return webrtc::CreateEnvironment();
  }
  return webrtc::CreateEnvironment(
      std::make_unique<CompositeFieldTrials>(std::move(views)));
}

}  // namespace

class PeerConnectionObserver;

PeerConnectionFactory::PeerConnectionFactory(
    std::shared_ptr<RtcRuntime> rtc_runtime)
    : PeerConnectionFactory(std::move(rtc_runtime), false, false) {}

PeerConnectionFactory::PeerConnectionFactory(
    std::shared_ptr<RtcRuntime> rtc_runtime,
    bool zero_playout_delay)
    : PeerConnectionFactory(std::move(rtc_runtime), zero_playout_delay, false) {}

PeerConnectionFactory::PeerConnectionFactory(
    std::shared_ptr<RtcRuntime> rtc_runtime,
    bool zero_playout_delay,
    bool enable_warp)
    : rtc_runtime_(rtc_runtime),
      env_(CreateEnvironment(zero_playout_delay, enable_warp)) {
  webrtc::PeerConnectionFactoryDependencies dependencies;
  dependencies.network_thread = rtc_runtime_->network_thread();
  dependencies.worker_thread = rtc_runtime_->worker_thread();
  dependencies.signaling_thread = rtc_runtime_->signaling_thread();
  dependencies.socket_factory = rtc_runtime_->network_thread()->socketserver();
  dependencies.event_log_factory = std::make_unique<webrtc::RtcEventLogFactory>();
  dependencies.env = env_;

  if (zero_playout_delay) {
    RTC_LOG(LS_INFO) << "WebRTC zero playout delay enabled with field trial: "
                     << kForcePlayoutDelayFieldTrial;
  }

  if (enable_warp) {
    RTC_LOG(LS_INFO) << "WebRTC WARP: SPED enabled via field trial "
                        "WebRTC-IceHandshakeDtls/Enabled/ (SNAP via "
                        "RtcConfiguration.enable_sctp_snap)";
  }

  // Create AdmProxy - it creates and initializes Platform ADM internally
  adm_proxy_ = rtc_runtime_->worker_thread()->BlockingCall([&] {
    return webrtc::make_ref_counted<livekit_ffi::AdmProxy>(
        env_, rtc_runtime_->worker_thread());
  });
  audio_device_ =
      std::make_shared<AudioDeviceController>(rtc_runtime_, adm_proxy_);

  dependencies.adm = adm_proxy_;

  auto video_encoder_factory =
      std::make_unique<livekit_ffi::VideoEncoderFactory>();
  video_encoder_factory_ = video_encoder_factory.get();
  dependencies.video_encoder_factory = std::move(video_encoder_factory);
  dependencies.video_decoder_factory =
      std::move(std::make_unique<livekit_ffi::VideoDecoderFactory>());
  dependencies.audio_encoder_factory = webrtc::CreateBuiltinAudioEncoderFactory();
  dependencies.audio_decoder_factory = webrtc::CreateBuiltinAudioDecoderFactory();
  // PERSEPHONE PATCH (ADR-0062): the module is built here and handed to the
  // voice engine, so the microphone gain can reach it. It is the module that
  // BuiltinAudioProcessingBuilder would have built.
  audio_processing_ = webrtc::BuiltinAudioProcessingBuilder().Build(env_);
  if (audio_processing_) {
    dependencies.audio_processing_builder =
        webrtc::CustomAudioProcessing(audio_processing_);
  } else {
    dependencies.audio_processing_builder =
        std::make_unique<webrtc::BuiltinAudioProcessingBuilder>();
  }

  webrtc::EnableMedia(dependencies);
  peer_factory_ =
      webrtc::CreateModularPeerConnectionFactory(std::move(dependencies));

  if (peer_factory_.get() == nullptr) {
    RTC_LOG_ERR(LS_ERROR) << "Failed to create PeerConnectionFactory";
    return;
  }

  // PERSEPHONE PATCH (ADR-0062): join the registry and take the gain that is
  // set now, so a gain chosen before the factory exists still applies.
  CaptureGainRegistry& registry = capture_gain_registry();
  std::lock_guard<std::mutex> lock(registry.mutex);
  registry.factories.push_back(this);
  apply_capture_gain(registry.gain_percent);
}

PeerConnectionFactory::~PeerConnectionFactory() {
  RTC_LOG(LS_VERBOSE) << "PeerConnectionFactory::~PeerConnectionFactory()";

  // PERSEPHONE PATCH (ADR-0062): leave the registry first, so no gain change
  // reaches a factory that is being torn down.
  {
    CaptureGainRegistry& registry = capture_gain_registry();
    std::lock_guard<std::mutex> lock(registry.mutex);
    registry.factories.erase(std::remove(registry.factories.begin(),
                                         registry.factories.end(), this),
                             registry.factories.end());
  }
  audio_processing_ = nullptr;
  peer_factory_ = nullptr;
  audio_device_ = nullptr;
  rtc_runtime_->worker_thread()->BlockingCall(
      [this] { adm_proxy_ = nullptr; });
}

std::shared_ptr<PeerConnection> PeerConnectionFactory::create_peer_connection(
    RtcConfiguration config,
    rust::Box<PeerConnectionObserverWrapper> observer) const {
  std::shared_ptr<PeerConnection> pc = std::make_shared<PeerConnection>(
      rtc_runtime_, peer_factory_, std::move(observer));

  if (!pc->Initialize(to_native_rtc_configuration(config))) {
    throw std::runtime_error(serialize_error(to_error(webrtc::RTCError(
        webrtc::RTCErrorType::INTERNAL_ERROR, "failed to initialize pc"))));
  }

  return pc;
}

std::shared_ptr<VideoTrack> PeerConnectionFactory::create_video_track(
    rust::String label,
    std::shared_ptr<VideoTrackSource> source) const {
  return std::static_pointer_cast<VideoTrack>(
      rtc_runtime_->get_or_create_media_stream_track(
          peer_factory_->CreateVideoTrack(source->get(), label.c_str())));
}

std::shared_ptr<AudioTrack> PeerConnectionFactory::create_audio_track(
    rust::String label,
    std::shared_ptr<AudioTrackSource> source) const {
  return std::static_pointer_cast<AudioTrack>(
      rtc_runtime_->get_or_create_media_stream_track(
          peer_factory_->CreateAudioTrack(label.c_str(), source->get().get())));
}

std::shared_ptr<AudioTrack> PeerConnectionFactory::create_device_audio_track(
    rust::String label) const {
  // Create an audio source that uses the ADM for capture
  webrtc::AudioOptions audio_options;
  audio_options.echo_cancellation = true;
  audio_options.auto_gain_control = true;
  audio_options.noise_suppression = true;

  webrtc::scoped_refptr<webrtc::AudioSourceInterface> audio_source =
      peer_factory_->CreateAudioSource(audio_options);

  if (!audio_source) {
    RTC_LOG(LS_ERROR) << "Failed to create device audio source";
    return nullptr;
  }

  return std::static_pointer_cast<AudioTrack>(
      rtc_runtime_->get_or_create_media_stream_track(
          peer_factory_->CreateAudioTrack(label.c_str(), audio_source.get())));
}

RtpCapabilities PeerConnectionFactory::rtp_sender_capabilities(
    MediaType type) const {
  return to_rust_rtp_capabilities(peer_factory_->GetRtpSenderCapabilities(
      static_cast<webrtc::MediaType>(type)));
}

RtpCapabilities PeerConnectionFactory::rtp_receiver_capabilities(
    MediaType type) const {
  return to_rust_rtp_capabilities(peer_factory_->GetRtpReceiverCapabilities(
      static_cast<webrtc::MediaType>(type)));
}

rust::Vec<rust::String> PeerConnectionFactory::video_encoder_backend_codecs(
    VideoEncoderBackend backend) const {
  rust::Vec<rust::String> codecs;
  if (!peer_factory_) {
    return codecs;
  }
  for (const auto& codec : video_encoder_factory_->BackendCodecs(backend)) {
    codecs.push_back(codec);
  }
  return codecs;
}

// PERSEPHONE PATCH (ADR-0062). The voice engine reads the module's
// configuration, changes some fields and applies it again on the worker
// thread (`ApplyOptions`), so this does the same there and no change is
// lost between the read and the write. The engine leaves
// `capture_level_adjustment` alone.
void PeerConnectionFactory::apply_capture_gain(uint16_t percent) const {
  if (!audio_processing_ || percent > kMaxCaptureGainPercent) {
    return;
  }
  const float factor = static_cast<float>(percent) / 100.0f;
  rtc_runtime_->worker_thread()->BlockingCall([this, factor] {
    webrtc::AudioProcessing::Config config = audio_processing_->GetConfig();
    config.capture_level_adjustment.enabled = factor != 1.0f;
    config.capture_level_adjustment.pre_gain_factor = factor;
    audio_processing_->ApplyConfig(config);
  });
}

CaptureProcessing PeerConnectionFactory::capture_processing() const {
  CaptureProcessing state{};
  state.gain_percent = 100;
  state.echo_requested = state.echo_active = -1;
  state.noise_requested = state.noise_active = -1;
  state.agc_requested = state.agc_active = -1;
  if (!audio_processing_ || !peer_factory_) {
    return state;
  }
  state.found = true;
  rtc_runtime_->worker_thread()->BlockingCall([this, &state] {
    const webrtc::AudioProcessing::Config config = audio_processing_->GetConfig();
    if (config.capture_level_adjustment.enabled) {
      state.gain_percent = static_cast<uint32_t>(
          std::lround(config.capture_level_adjustment.pre_gain_factor * 100.0f));
    }
  });
  // The audio device module's own state, read outside the lambda above: the
  // proxy marshals each call to the worker thread itself.
  state.playout_initialized = adm_proxy_->PlayoutIsInitialized();
  state.playing = adm_proxy_->Playing();
  state.recording = adm_proxy_->Recording();
  const webrtc::AudioProcessingState reported =
      peer_factory_->GetAudioProcessingState();
  state.echo_requested = tri_state(reported.echo_cancellation.requested_enabled);
  state.echo_active = tri_state(reported.echo_cancellation.software_active);
  state.noise_requested = tri_state(reported.noise_suppression.requested_enabled);
  state.noise_active = tri_state(reported.noise_suppression.software_active);
  state.agc_requested = tri_state(reported.auto_gain_control.requested_enabled);
  state.agc_active = tri_state(reported.auto_gain_control.software_active);
  return state;
}

bool set_capture_gain_percent(uint16_t percent) {
  if (percent > kMaxCaptureGainPercent) {
    return false;
  }
  CaptureGainRegistry& registry = capture_gain_registry();
  std::lock_guard<std::mutex> lock(registry.mutex);
  registry.gain_percent = percent;
  for (const PeerConnectionFactory* factory : registry.factories) {
    factory->apply_capture_gain(percent);
  }
  return true;
}

CaptureProcessing capture_processing_state() {
  CaptureGainRegistry& registry = capture_gain_registry();
  std::lock_guard<std::mutex> lock(registry.mutex);
  if (registry.factories.empty()) {
    CaptureProcessing none{};
    none.gain_percent = 100;
    none.echo_requested = none.echo_active = -1;
    none.noise_requested = none.noise_active = -1;
    none.agc_requested = none.agc_active = -1;
    return none;
  }
  return registry.factories.back()->capture_processing();
}

std::shared_ptr<AudioDeviceController> PeerConnectionFactory::audio_device() const {
  return audio_device_;
}

bool PeerConnectionFactory::zero_playout_delay_enabled() const {
  return env_.field_trials().Lookup("WebRTC-ForcePlayoutDelay") ==
         kForcePlayoutDelayValue;
}

std::shared_ptr<PeerConnectionFactory> create_peer_connection_factory() {
  return std::make_shared<PeerConnectionFactory>(RtcRuntime::create());
}

std::shared_ptr<PeerConnectionFactory>
create_peer_connection_factory_with_zero_playout_delay() {
  return std::make_shared<PeerConnectionFactory>(RtcRuntime::create(), true);
}

std::shared_ptr<PeerConnectionFactory>
create_peer_connection_factory_with_options(bool zero_playout_delay,
                                            bool enable_warp) {
  return std::make_shared<PeerConnectionFactory>(RtcRuntime::create(),
                                                 zero_playout_delay,
                                                 enable_warp);
}

}  // namespace livekit_ffi
