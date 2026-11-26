//! The own camera's video effects (ADR-0068): a background behind the person
//! (a blur or a picture drawn from code), then a colour filter on the whole
//! picture. Works in place on the I420 picture that goes to both the own
//! preview and the SFU, so others see what the preview shows.
//!
//! No I/O, no LiveKit and no OS types. Every size is checked before a buffer
//! is made, and the scratch buffers are kept for the next picture of the
//! same size.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use persephone_call_protocol::{
    Background, BackgroundPicture, BlurStrength, ColourFilter, MAX_VIDEO_PIXELS, MAX_VIDEO_SIDE,
    VideoEffect,
};

use crate::segmenter::Segmenter;

/// Box blur passes in each direction. Three passes of a box come close to a
/// Gaussian blur, and each costs the same at any radius.
const BLUR_PASSES: usize = 3;
/// The blur radius is `strength x width / BLUR_RADIUS_DIVISOR` pixels: 2 to
/// 20 pixels at 640 wide (ADR-0068).
const BLUR_RADIUS_DIVISOR: u32 = 3200;
/// The colour shift of the warm and cool filters, in chroma steps.
const WARM_SHIFT: i16 = 10;
const WARM_COUNTER_SHIFT: i16 = 8;
/// The power curve of the brighten filter on the luma range: below 1 lifts
/// the dark parts and keeps black and white.
const BRIGHTEN_GAMMA: f32 = 0.7;
/// Video range luma: black and white (BT.601).
const LUMA_BLACK: u8 = 16;
const LUMA_WHITE: u8 = 235;
/// Neutral chroma.
const CHROMA_ZERO: u8 = 128;

/// Whether the worker can apply `effect`: an effect that needs the person
/// mask needs a segmenter.
pub const fn can_apply(effect: VideoEffect, segmenter_available: bool) -> bool {
    !effect.needs_mask() || segmenter_available
}

/// The effect to use after a `SetVideoEffect` of `requested`, and whether
/// the request was refused: an effect that cannot run is refused, and
/// `current` stays (ADR-0068).
pub const fn accept_effect(
    current: VideoEffect,
    requested: VideoEffect,
    segmenter_available: bool,
) -> (VideoEffect, bool) {
    if can_apply(requested, segmenter_available) {
        (requested, false)
    } else {
        (current, true)
    }
}

/// The effect in use, shared with the camera's capture thread. The event
/// loop stores it; the frame callback loads it for each picture. One atomic
/// value: no lock and no queue.
pub struct SharedEffect(AtomicU32);

impl SharedEffect {
    pub fn new(effect: VideoEffect) -> Arc<Self> {
        Arc::new(Self(AtomicU32::new(pack(effect))))
    }

    pub fn store(&self, effect: VideoEffect) {
        self.0.store(pack(effect), Ordering::Release);
    }

    /// The effect in use. `None` only for a value that no `VideoEffect`
    /// gives; the caller then drops the picture.
    pub fn load(&self) -> Option<VideoEffect> {
        let [_, kind, value, filter] = self.0.load(Ordering::Acquire).to_be_bytes();
        VideoEffect::from_bytes([kind, value, filter]).ok()
    }
}

fn pack(effect: VideoEffect) -> u32 {
    let [kind, value, filter] = effect.to_bytes();
    u32::from_be_bytes([0, kind, value, filter])
}

/// The size of a picture's chroma planes.
const fn chroma_size(width: usize, height: usize) -> (usize, usize) {
    (width.div_ceil(2), height.div_ceil(2))
}

/// Whether `len` bytes hold `rows` rows of `width` bytes at `stride`.
fn plane_fits(len: usize, width: usize, rows: usize, stride: usize) -> bool {
    stride >= width
        && stride
            .checked_mul(rows - 1)
            .and_then(|start| start.checked_add(width))
            .is_some_and(|needed| needed <= len)
}

/// One I420 picture's planes, borrowed for processing. Made only by
/// [`Planes::new`], which checks the size and that each plane holds its
/// rows.
pub struct Planes<'a> {
    width: usize,
    height: usize,
    y: &'a mut [u8],
    y_stride: usize,
    u: &'a mut [u8],
    u_stride: usize,
    v: &'a mut [u8],
    v_stride: usize,
}

impl<'a> Planes<'a> {
    /// The planes of a `width` x `height` picture, or `None` when it is
    /// larger than a camera picture (640x360, ADR-0048), smaller than 2x2,
    /// or a plane is too short for its stride.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        y: &'a mut [u8],
        y_stride: u32,
        u: &'a mut [u8],
        u_stride: u32,
        v: &'a mut [u8],
        v_stride: u32,
    ) -> Option<Self> {
        if !(2..=MAX_VIDEO_SIDE).contains(&width)
            || !(2..=MAX_VIDEO_SIDE).contains(&height)
            || u64::from(width) * u64::from(height) > u64::from(MAX_VIDEO_PIXELS)
        {
            return None;
        }
        let (width, height) = (width as usize, height as usize);
        let (y_stride, u_stride, v_stride) =
            (y_stride as usize, u_stride as usize, v_stride as usize);
        let (chroma_width, chroma_height) = chroma_size(width, height);
        let fits = plane_fits(y.len(), width, height, y_stride)
            && plane_fits(u.len(), chroma_width, chroma_height, u_stride)
            && plane_fits(v.len(), chroma_width, chroma_height, v_stride);
        fits.then_some(Self {
            width,
            height,
            y,
            y_stride,
            u,
            u_stride,
            v,
            v_stride,
        })
    }

    /// The three planes with their sizes and strides, for one loop over
    /// each.
    fn planes_mut(&mut self) -> [PlaneMut<'_>; 3] {
        let (chroma_width, chroma_height) = chroma_size(self.width, self.height);
        [
            PlaneMut {
                data: self.y,
                width: self.width,
                height: self.height,
                stride: self.y_stride,
            },
            PlaneMut {
                data: self.u,
                width: chroma_width,
                height: chroma_height,
                stride: self.u_stride,
            },
            PlaneMut {
                data: self.v,
                width: chroma_width,
                height: chroma_height,
                stride: self.v_stride,
            },
        ]
    }
}

/// One plane of a [`Planes`].
struct PlaneMut<'p> {
    data: &'p mut [u8],
    width: usize,
    height: usize,
    stride: usize,
}

impl PlaneMut<'_> {
    fn rows_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        let width = self.width;
        self.data
            .chunks_mut(self.stride)
            .take(self.height)
            .map(move |row| &mut row[..width])
    }

    fn rows(&self) -> impl Iterator<Item = &[u8]> {
        self.data
            .chunks(self.stride)
            .take(self.height)
            .map(|row| &row[..self.width])
    }
}

/// A person mask: one byte for each pixel, 0 for background and 255 for the
/// person. At most a camera picture's size.
#[derive(Default)]
pub struct Mask {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl Mask {
    /// Makes the mask `width` x `height`, or refuses a size outside 1x1 to a
    /// camera picture's limits. The bytes are then the segmenter's to write.
    #[allow(dead_code)] // the segmenter's side (ADR-0068); this build has none
    pub fn resize(&mut self, width: u32, height: u32) -> bool {
        if !(1..=MAX_VIDEO_SIDE).contains(&width)
            || !(1..=MAX_VIDEO_SIDE).contains(&height)
            || u64::from(width) * u64::from(height) > u64::from(MAX_VIDEO_PIXELS)
        {
            return false;
        }
        self.width = width as usize;
        self.height = height as usize;
        self.data.resize(self.width * self.height, 0);
        true
    }

    /// The mask's bytes, row by row with no padding.
    #[allow(dead_code)] // the segmenter's side (ADR-0068); this build has none
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// An effect that needs the person mask got none: the picture must not go
/// out.
#[derive(Debug, Eq, PartialEq)]
pub struct NoMask;

/// A background drawn for one picture size.
struct Drawn {
    picture: BackgroundPicture,
    width: usize,
    height: usize,
    planes: [Vec<u8>; 3],
}

/// Applies video effects, keeping its scratch planes between pictures.
pub struct Effects {
    /// The blurred or drawn background of each plane, with no padding.
    background: [Vec<u8>; 3],
    /// The mask scaled to the luma and to the chroma size.
    alpha: [Vec<u8>; 2],
    /// The mask columns that each output column reads, for `scale_mask`.
    columns: Vec<(usize, usize, f32)>,
    /// Blur scratch.
    scratch: Vec<u8>,
    sums: Vec<u32>,
    drawn: Option<Drawn>,
    brighten: [u8; 256],
}

impl Default for Effects {
    fn default() -> Self {
        Self::new()
    }
}

impl Effects {
    pub fn new() -> Self {
        Self {
            background: Default::default(),
            alpha: Default::default(),
            columns: Vec::new(),
            scratch: Vec::new(),
            sums: Vec::new(),
            drawn: None,
            brighten: brighten_curve(),
        }
    }

    /// Applies `effect` to `planes` in place: the background with `mask`,
    /// then the colour filter. An effect that needs a mask without one
    /// changes nothing and returns [`NoMask`].
    pub fn apply(
        &mut self,
        effect: VideoEffect,
        planes: &mut Planes<'_>,
        mask: Option<&Mask>,
    ) -> Result<(), NoMask> {
        match effect.background {
            Background::None => {}
            background => {
                let mask = mask.filter(|mask| !mask.is_empty()).ok_or(NoMask)?;
                self.replace_background(background, planes, mask)?;
            }
        }
        self.filter(effect.filter, planes);
        Ok(())
    }

    /// Mixes the blurred or drawn background into `planes` where `mask`
    /// says background. [`NoMask`] when a plane could not be mixed: the
    /// picture is then not sent.
    fn replace_background(
        &mut self,
        background: Background,
        planes: &mut Planes<'_>,
        mask: &Mask,
    ) -> Result<(), NoMask> {
        let (width, height) = (planes.width, planes.height);
        let (chroma_width, chroma_height) = chroma_size(width, height);
        scale_mask(mask, width, height, &mut self.alpha[0], &mut self.columns);
        scale_mask(
            mask,
            chroma_width,
            chroma_height,
            &mut self.alpha[1],
            &mut self.columns,
        );
        match background {
            Background::None => return Ok(()),
            Background::Blur(strength) => self.blur(planes, strength),
            Background::Picture(picture) => self.draw(picture, width, height),
        }
        let drawn = self.drawn.as_ref().map(|drawn| &drawn.planes);
        for (index, mut plane) in planes.planes_mut().into_iter().enumerate() {
            let alpha = &self.alpha[usize::from(index > 0)];
            let background = match background {
                Background::Picture(_) => drawn.map_or(&[][..], |planes| &planes[index][..]),
                _ => &self.background[index][..],
            };
            if !composite(&mut plane, alpha, background) {
                return Err(NoMask);
            }
        }
        Ok(())
    }

    /// Blurs each plane of `planes` into `self.background`.
    fn blur(&mut self, planes: &mut Planes<'_>, strength: BlurStrength) {
        let luma_radius = blur_radius(strength, planes.width);
        let chroma_radius = (luma_radius / 2).max(1);
        for (index, plane) in planes.planes_mut().into_iter().enumerate() {
            let radius = if index == 0 {
                luma_radius
            } else {
                chroma_radius
            };
            let out = &mut self.background[index];
            out.clear();
            out.reserve(plane.width * plane.height);
            for row in plane.rows() {
                out.extend_from_slice(row);
            }
            box_blur(
                out,
                &mut self.scratch,
                &mut self.sums,
                plane.width,
                plane.height,
                radius,
            );
        }
    }

    /// Draws `picture` at this size, unless it is drawn already.
    fn draw(&mut self, picture: BackgroundPicture, width: usize, height: usize) {
        if self.drawn.as_ref().is_some_and(|drawn| {
            (drawn.picture, drawn.width, drawn.height) == (picture, width, height)
        }) {
            return;
        }
        let mut planes = match self.drawn.take() {
            Some(drawn) => drawn.planes,
            None => Default::default(),
        };
        draw_picture(picture, width, height, &mut planes);
        self.drawn = Some(Drawn {
            picture,
            width,
            height,
            planes,
        });
    }

    fn filter(&self, filter: ColourFilter, planes: &mut Planes<'_>) {
        let [mut luma, mut u, mut v] = planes.planes_mut();
        match filter {
            ColourFilter::None => {}
            ColourFilter::Greyscale => {
                for plane in [&mut u, &mut v] {
                    for row in plane.rows_mut() {
                        row.fill(CHROMA_ZERO);
                    }
                }
            }
            ColourFilter::Warm => {
                shift(&mut u, -WARM_COUNTER_SHIFT);
                shift(&mut v, WARM_SHIFT);
            }
            ColourFilter::Cool => {
                shift(&mut u, WARM_SHIFT);
                shift(&mut v, -WARM_COUNTER_SHIFT);
            }
            ColourFilter::Brighten => {
                for row in luma.rows_mut() {
                    for value in row {
                        *value = self.brighten[usize::from(*value)];
                    }
                }
            }
        }
    }
}

/// The blur radius in pixels for a picture `width` pixels wide: at least 1.
fn blur_radius(strength: BlurStrength, width: usize) -> usize {
    let radius = u32::from(strength.percent()) * width as u32 / BLUR_RADIUS_DIVISOR;
    (radius as usize).max(1)
}

/// Adds `by` to every byte of a chroma plane, kept in 0..=255.
fn shift(plane: &mut PlaneMut<'_>, by: i16) {
    for row in plane.rows_mut() {
        for value in row {
            *value = (i16::from(*value) + by).clamp(0, 255) as u8;
        }
    }
}

/// The brighten filter's luma curve: a power curve on the video range, with
/// black and white kept and every value outside the range unchanged.
fn brighten_curve() -> [u8; 256] {
    let mut curve = [0; 256];
    let range = f32::from(LUMA_WHITE - LUMA_BLACK);
    for (value, out) in curve.iter_mut().enumerate() {
        let value = value as u8;
        *out = if (LUMA_BLACK..=LUMA_WHITE).contains(&value) {
            let level = f32::from(value - LUMA_BLACK) / range;
            (f32::from(LUMA_BLACK) + range * level.powf(BRIGHTEN_GAMMA)).round() as u8
        } else {
            value
        };
    }
    curve
}

/// Mixes `background` into `plane` where the mask says background:
/// `alpha` 255 keeps the plane, 0 takes the background. Both are `width`
/// x `height` with no padding. `false`, with nothing changed, when either
/// is too short.
fn composite(plane: &mut PlaneMut<'_>, alpha: &[u8], background: &[u8]) -> bool {
    let width = plane.width;
    if alpha.len() < width * plane.height || background.len() < width * plane.height {
        return false;
    }
    for (row_index, row) in plane.rows_mut().enumerate() {
        let start = row_index * width;
        let alpha = &alpha[start..start + width];
        let background = &background[start..start + width];
        for ((value, &a), &back) in row.iter_mut().zip(alpha).zip(background) {
            let (a, own, back) = (u32::from(a), u32::from(*value), u32::from(back));
            *value = ((a * own + (255 - a) * back + 127) / 255) as u8;
        }
    }
    true
}

/// Scales `mask` to `width` x `height` into `out`, bilinear, with pixel
/// centres aligned. `columns` is scratch, kept between pictures.
fn scale_mask(
    mask: &Mask,
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
    columns: &mut Vec<(usize, usize, f32)>,
) {
    out.resize(width * height, 0);
    if (mask.width, mask.height) == (width, height) {
        out.copy_from_slice(&mask.data);
        return;
    }
    // The source position of each output column, once for all rows.
    let column = |x: usize, from: usize, to: usize| -> (usize, usize, f32) {
        let position =
            ((x as f32 + 0.5) * from as f32 / to as f32 - 0.5).clamp(0.0, (from - 1) as f32);
        let first = position.floor() as usize;
        (first, (first + 1).min(from - 1), position - first as f32)
    };
    columns.clear();
    columns.extend((0..width).map(|x| column(x, mask.width, width)));
    for (y, out_row) in out.chunks_mut(width).enumerate() {
        let (top, bottom, down) = column(y, mask.height, height);
        let top = &mask.data[top * mask.width..][..mask.width];
        let bottom = &mask.data[bottom * mask.width..][..mask.width];
        for (value, &(left, right, across)) in out_row.iter_mut().zip(columns.iter()) {
            let upper = f32::from(top[left]) * (1.0 - across) + f32::from(top[right]) * across;
            let lower =
                f32::from(bottom[left]) * (1.0 - across) + f32::from(bottom[right]) * across;
            *value = (upper * (1.0 - down) + lower * down).round() as u8;
        }
    }
}

/// Blurs a `width` x `height` plane with no padding in place: three box
/// passes across, then three down, with the edge pixels repeated.
fn box_blur(
    plane: &mut [u8],
    scratch: &mut Vec<u8>,
    sums: &mut Vec<u32>,
    width: usize,
    height: usize,
    radius: usize,
) {
    scratch.resize(plane.len(), 0);
    for _ in 0..BLUR_PASSES {
        box_across(plane, scratch, width, radius);
        plane.copy_from_slice(scratch);
    }
    for _ in 0..BLUR_PASSES {
        box_down(plane, scratch, sums, width, height, radius);
        plane.copy_from_slice(scratch);
    }
}

/// One box pass along each row of `from` into `to`.
fn box_across(from: &[u8], to: &mut [u8], width: usize, radius: usize) {
    let span = (2 * radius + 1) as u32;
    let last = width - 1;
    for (row, out) in from.chunks(width).zip(to.chunks_mut(width)) {
        let mut sum: u32 = (0..=2 * radius)
            .map(|k| u32::from(row[k.saturating_sub(radius).min(last)]))
            .sum();
        for (x, value) in out.iter_mut().enumerate() {
            *value = ((sum + span / 2) / span) as u8;
            sum += u32::from(row[(x + radius + 1).min(last)]);
            sum -= u32::from(row[x.saturating_sub(radius)]);
        }
    }
}

/// One box pass down each column of `from` into `to`, row by row.
fn box_down(
    from: &[u8],
    to: &mut [u8],
    sums: &mut Vec<u32>,
    width: usize,
    height: usize,
    radius: usize,
) {
    let span = (2 * radius + 1) as u32;
    let last = height - 1;
    let row = |y: usize| &from[y * width..][..width];
    sums.clear();
    sums.resize(width, 0);
    for k in 0..=2 * radius {
        for (sum, &value) in sums.iter_mut().zip(row(k.saturating_sub(radius).min(last))) {
            *sum += u32::from(value);
        }
    }
    for (y, out) in to.chunks_mut(width).take(height).enumerate() {
        for (value, &sum) in out.iter_mut().zip(sums.iter()) {
            *value = ((sum + span / 2) / span) as u8;
        }
        let (add, remove) = (
            row((y + radius + 1).min(last)),
            row(y.saturating_sub(radius)),
        );
        for ((sum, &add), &remove) in sums.iter_mut().zip(add).zip(remove) {
            *sum = *sum + u32::from(add) - u32::from(remove);
        }
    }
}

/// Draws `picture` at `width` x `height` into three I420 planes with no
/// padding.
fn draw_picture(
    picture: BackgroundPicture,
    width: usize,
    height: usize,
    planes: &mut [Vec<u8>; 3],
) {
    let aspect = width as f32 / height as f32;
    let (chroma_width, chroma_height) = chroma_size(width, height);
    let [luma, u, v] = planes;
    luma.clear();
    for y in 0..height {
        for x in 0..width {
            let (fx, fy) = (
                (x as f32 + 0.5) / width as f32,
                (y as f32 + 0.5) / height as f32,
            );
            luma.push(to_yuv(colour(picture, fx, fy, aspect))[0]);
        }
    }
    u.clear();
    v.clear();
    for y in 0..chroma_height {
        for x in 0..chroma_width {
            // The centre of the 2x2 block of luma pixels.
            let fx = ((2 * x + 1) as f32 / width as f32).min(1.0);
            let fy = ((2 * y + 1) as f32 / height as f32).min(1.0);
            let [_, chroma_u, chroma_v] = to_yuv(colour(picture, fx, fy, aspect));
            u.push(chroma_u);
            v.push(chroma_v);
        }
    }
}

type Rgb = [f32; 3];

fn mix(from: Rgb, to: Rgb, amount: f32) -> Rgb {
    let amount = amount.clamp(0.0, 1.0);
    [0, 1, 2].map(|i| from[i] + (to[i] - from[i]) * amount)
}

/// A soft round light at (`cx`, `cy`), about `size` wide: 1 at its centre.
fn glow(fx: f32, fy: f32, aspect: f32, (cx, cy): (f32, f32), size: f32) -> f32 {
    let (dx, dy) = ((fx - cx) * aspect, fy - cy);
    (-(dx * dx + dy * dy) / (size * size)).exp()
}

/// The colour of `picture` at (`fx`, `fy`), each 0 to 1 from the top left.
fn colour(picture: BackgroundPicture, fx: f32, fy: f32, aspect: f32) -> Rgb {
    match picture {
        BackgroundPicture::Dusk => {
            let sky = if fy < 0.55 {
                mix([36.0, 40.0, 90.0], [170.0, 85.0, 120.0], fy / 0.55)
            } else {
                mix(
                    [170.0, 85.0, 120.0],
                    [240.0, 150.0, 80.0],
                    (fy - 0.55) / 0.45,
                )
            };
            let sun = glow(fx, fy, aspect, (0.5, 0.82), 0.22);
            [0, 1, 2].map(|i| sky[i] + sun * [60.0, 45.0, 15.0][i])
        }
        BackgroundPicture::Sea => {
            const HORIZON: f32 = 0.45;
            let sky = mix([120.0, 170.0, 220.0], [205.0, 225.0, 240.0], fy / HORIZON);
            let water = mix(
                [40.0, 120.0, 150.0],
                [10.0, 45.0, 85.0],
                (fy - HORIZON) / (1.0 - HORIZON),
            );
            let wave = 8.0 * (fy * 60.0 + 3.0 * (fx * 6.0).sin()).sin() * (fy - HORIZON).max(0.0);
            let water = water.map(|c| c + wave);
            // A soft horizon, two percent of the height.
            mix(sky, water, (fy - HORIZON) / 0.02 + 0.5)
        }
        BackgroundPicture::Forest => {
            let base = mix([25.0, 60.0, 35.0], [55.0, 95.0, 50.0], fy);
            let trunk = 0.5 + 0.5 * (fx * 25.0 + 2.0 * (fx * 7.0).sin()).sin();
            let shade = 1.0 - 0.35 * trunk.powi(4);
            let light = glow(fx, fy, aspect, (0.2, 0.1), 0.35);
            [0, 1, 2].map(|i| base[i] * shade + light * [30.0, 35.0, 15.0][i])
        }
        BackgroundPicture::Studio => {
            let (dx, dy) = ((fx - 0.5) * aspect, fy - 0.45);
            let level = 165.0 - 95.0 * ((dx * dx + dy * dy).sqrt() / 0.9).clamp(0.0, 1.0);
            [level, level, level + 6.0]
        }
    }
}

/// An RGB colour (0 to 255) as video range BT.601 Y, U and V.
fn to_yuv([r, g, b]: Rgb) -> [u8; 3] {
    let [r, g, b] = [r, g, b].map(|c| c.clamp(0.0, 255.0));
    let y = 16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0;
    let u = 128.0 + (-37.797 * r - 74.203 * g + 112.0 * b) / 255.0;
    let v = 128.0 + (112.0 * r - 93.786 * g - 18.214 * b) / 255.0;
    [y, u, v].map(|value| value.round().clamp(0.0, 255.0) as u8)
}

/// The effects of one camera feed: the shared effect in use, this feed's
/// segmenter, if any, and the scratch planes. The camera's frame callback
/// (and the camera test's) owns one.
pub struct CameraEffects {
    effect: Arc<SharedEffect>,
    segmenter: Option<Box<dyn Segmenter>>,
    mask: Mask,
    effects: Effects,
}

impl CameraEffects {
    pub fn new(effect: Arc<SharedEffect>, segmenter: Option<Box<dyn Segmenter>>) -> Self {
        Self {
            effect,
            segmenter,
            mask: Mask::default(),
            effects: Effects::new(),
        }
    }

    /// Whether the effect in use is none, so a picture goes out as it is
    /// and needs no [`Planes`]. `false` also for a value that no
    /// `VideoEffect` gives.
    pub fn leaves_plain(&self) -> bool {
        self.effect.load() == Some(VideoEffect::NONE)
    }

    /// Applies the effect in use to one picture. `false` when the picture
    /// must not go out: the effect needs a mask that the segmenter did not
    /// give (fail closed, ADR-0068).
    pub fn process(&mut self, planes: &mut Planes<'_>) -> bool {
        let Some(effect) = self.effect.load() else {
            return false;
        };
        let mask = if effect.needs_mask() {
            let made = self
                .segmenter
                .as_mut()
                .is_some_and(|segmenter| segmenter.mask(planes, &mut self.mask));
            if !made {
                return false;
            }
            Some(&self.mask)
        } else {
            None
        };
        self.effects.apply(effect, planes, mask).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test picture's planes, owned, with padding at the end of each row.
    struct Picture {
        width: u32,
        height: u32,
        stride: u32,
        chroma_stride: u32,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    }

    impl Picture {
        /// Luma from `luma(x, y)`, chroma `u` and `v`. Padding bytes are 7.
        fn new(width: u32, height: u32, luma: impl Fn(usize, usize) -> u8, u: u8, v: u8) -> Self {
            let (stride, chroma_stride) = (width + 3, width.div_ceil(2) + 2);
            let (cw, ch) = chroma_size(width as usize, height as usize);
            let mut y = vec![7; (stride * height) as usize];
            for row in 0..height as usize {
                for x in 0..width as usize {
                    y[row * stride as usize + x] = luma(x, row);
                }
            }
            let chroma = |value: u8| {
                let mut plane = vec![7; chroma_stride as usize * ch];
                for row in 0..ch {
                    plane[row * chroma_stride as usize..][..cw].fill(value);
                }
                plane
            };
            Self {
                width,
                height,
                stride,
                chroma_stride,
                y,
                u: chroma(u),
                v: chroma(v),
            }
        }

        fn planes(&mut self) -> Planes<'_> {
            Planes::new(
                self.width,
                self.height,
                &mut self.y,
                self.stride,
                &mut self.u,
                self.chroma_stride,
                &mut self.v,
                self.chroma_stride,
            )
            .expect("valid planes")
        }

        fn luma(&self, x: usize, y: usize) -> u8 {
            self.y[y * self.stride as usize + x]
        }

        fn u_at(&self, x: usize, y: usize) -> u8 {
            self.u[y * self.chroma_stride as usize + x]
        }

        fn v_at(&self, x: usize, y: usize) -> u8 {
            self.v[y * self.chroma_stride as usize + x]
        }

        /// Every padding byte is still 7.
        fn padding_untouched(&self) -> bool {
            let (cw, ch) = chroma_size(self.width as usize, self.height as usize);
            let rows = |plane: &[u8], stride: usize, width: usize, height: usize| {
                plane
                    .chunks(stride)
                    .take(height)
                    .all(|row| row[width..].iter().all(|&b| b == 7))
            };
            rows(
                &self.y,
                self.stride as usize,
                self.width as usize,
                self.height as usize,
            ) && rows(&self.u, self.chroma_stride as usize, cw, ch)
                && rows(&self.v, self.chroma_stride as usize, cw, ch)
        }
    }

    fn checkerboard(x: usize, y: usize) -> u8 {
        if (x / 2 + y / 2).is_multiple_of(2) {
            40
        } else {
            200
        }
    }

    fn mask(width: u32, height: u32, value: impl Fn(usize, usize) -> u8) -> Mask {
        let mut mask = Mask::default();
        assert!(mask.resize(width, height));
        let w = width as usize;
        for (index, byte) in mask.data_mut().iter_mut().enumerate() {
            *byte = value(index % w, index / w);
        }
        mask
    }

    fn blur(percent: u8) -> VideoEffect {
        VideoEffect {
            background: Background::Blur(BlurStrength::new(percent).unwrap()),
            filter: ColourFilter::None,
        }
    }

    fn filter(filter: ColourFilter) -> VideoEffect {
        VideoEffect {
            background: Background::None,
            filter,
        }
    }

    fn picture(picture: BackgroundPicture) -> VideoEffect {
        VideoEffect {
            background: Background::Picture(picture),
            filter: ColourFilter::None,
        }
    }

    /// The spread of the luma values of a picture.
    fn luma_variance(picture: &Picture) -> f64 {
        let values: Vec<f64> = (0..picture.height as usize)
            .flat_map(|y| (0..picture.width as usize).map(move |x| (x, y)))
            .map(|(x, y)| f64::from(picture.luma(x, y)))
            .collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64
    }

    /// GIVEN planes larger than a camera picture, smaller than 2x2, or with
    /// a plane too short for its stride or a stride under the width
    /// WHEN they are checked
    /// THEN each is refused; the largest camera picture is accepted.
    #[test]
    fn given_bad_plane_sizes_when_checked_then_they_are_refused() {
        let check = |w: u32, h: u32, y_len: usize, y_stride: u32, c_len: usize, c_stride: u32| {
            let (mut y, mut u, mut v) = (vec![0; y_len], vec![0; c_len], vec![0; c_len]);
            Planes::new(w, h, &mut y, y_stride, &mut u, c_stride, &mut v, c_stride).is_some()
        };
        assert!(check(640, 360, 640 * 360, 640, 320 * 180, 320));
        assert!(!check(642, 360, 642 * 360, 642, 321 * 180, 321));
        assert!(!check(640, 362, 640 * 362, 640, 320 * 181, 320));
        assert!(!check(1, 4, 4, 1, 2, 1));
        assert!(!check(64, 36, 64 * 36 - 1, 64, 32 * 18, 32));
        assert!(!check(64, 36, 64 * 36, 64, 32 * 18 - 1, 32));
        assert!(!check(64, 36, 64 * 36, 63, 32 * 18, 32));
        // The last row needs only the width, not the whole stride.
        assert!(check(64, 36, 80 * 35 + 64, 80, 40 * 17 + 32, 40));
    }

    /// GIVEN no effect
    /// WHEN it is applied to a picture
    /// THEN no byte changes.
    #[test]
    fn given_no_effect_when_applied_then_the_picture_is_unchanged() {
        let mut picture = Picture::new(33, 19, checkerboard, 90, 170);
        let before = (picture.y.clone(), picture.u.clone(), picture.v.clone());
        let mut effects = Effects::new();
        assert_eq!(
            effects.apply(VideoEffect::NONE, &mut picture.planes(), None),
            Ok(())
        );
        assert_eq!(
            (picture.y.clone(), picture.u.clone(), picture.v.clone()),
            before
        );
    }

    /// GIVEN each colour filter
    /// WHEN it is applied
    /// THEN greyscale makes the chroma neutral and keeps the luma; warm adds
    /// red and takes blue; cool does the opposite; values stay in 0..=255;
    /// no padding byte changes.
    #[test]
    fn given_colour_filters_when_applied_then_the_colours_move_as_named() {
        let mut effects = Effects::new();
        let run = |effects: &mut Effects, filter_kind, u, v| {
            let mut picture = Picture::new(10, 6, |x, _| 30 + x as u8 * 10, u, v);
            effects
                .apply(filter(filter_kind), &mut picture.planes(), None)
                .unwrap();
            assert!(picture.padding_untouched(), "{filter_kind:?}");
            picture
        };
        let grey = run(&mut effects, ColourFilter::Greyscale, 60, 200);
        assert_eq!((grey.u_at(0, 0), grey.v_at(4, 2)), (128, 128));
        assert_eq!(grey.luma(3, 1), 60);
        let warm = run(&mut effects, ColourFilter::Warm, 128, 128);
        assert!(warm.v_at(1, 1) > 128 && warm.u_at(1, 1) < 128);
        let cool = run(&mut effects, ColourFilter::Cool, 128, 128);
        assert!(cool.u_at(1, 1) > 128 && cool.v_at(1, 1) < 128);
        assert_eq!(cool.luma(5, 5), 80);
        let clamped = run(&mut effects, ColourFilter::Warm, 3, 250);
        assert_eq!((clamped.u_at(0, 0), clamped.v_at(0, 0)), (0, 255));
    }

    /// GIVEN the brighten filter
    /// WHEN its curve is read
    /// THEN it keeps black, white and every value outside the video range,
    /// never darkens, lifts the dark parts most, and never decreases.
    #[test]
    fn given_brighten_when_applied_then_dark_parts_rise_and_ends_stay() {
        let curve = brighten_curve();
        assert_eq!((curve[16], curve[235]), (16, 235));
        assert!((0..16).chain(236..=255).all(|v| curve[v] == v as u8));
        assert!(
            curve
                .iter()
                .enumerate()
                .all(|(v, &out)| usize::from(out) >= v)
        );
        assert!(curve.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(curve[40] - 40 > curve[200] - 200);
        let mut picture = Picture::new(4, 4, |_, _| 40, 128, 128);
        Effects::new()
            .apply(filter(ColourFilter::Brighten), &mut picture.planes(), None)
            .unwrap();
        assert_eq!(picture.luma(2, 2), curve[40]);
        assert_eq!(picture.u_at(1, 1), 128);
    }

    /// GIVEN blur or a picture, and no mask or an empty one
    /// WHEN the effect is applied
    /// THEN it is refused and the picture is unchanged, so it can be dropped.
    #[test]
    fn given_background_effect_without_mask_when_applied_then_it_is_refused() {
        let mut effects = Effects::new();
        for effect in [blur(50), picture(BackgroundPicture::Sea)] {
            for empty in [None, Some(&Mask::default())] {
                let mut picture = Picture::new(16, 8, checkerboard, 100, 150);
                let before = picture.y.clone();
                assert_eq!(
                    effects.apply(effect, &mut picture.planes(), empty),
                    Err(NoMask)
                );
                assert_eq!(picture.y, before);
            }
        }
    }

    /// GIVEN blur with a mask that marks every pixel as the person
    /// WHEN it is applied
    /// THEN the picture is unchanged.
    #[test]
    fn given_blur_and_all_person_mask_when_applied_then_nothing_changes() {
        let mut picture = Picture::new(64, 36, checkerboard, 90, 170);
        let before = (picture.y.clone(), picture.u.clone(), picture.v.clone());
        let all = mask(32, 18, |_, _| 255);
        Effects::new()
            .apply(blur(100), &mut picture.planes(), Some(&all))
            .unwrap();
        assert_eq!(
            (picture.y.clone(), picture.u.clone(), picture.v.clone()),
            before
        );
    }

    /// GIVEN blur with a mask that marks every pixel as background
    /// WHEN it is applied at a weak and a strong strength
    /// THEN the pattern is smoothed, much more at the strong strength, its
    /// mean stays, a flat picture stays flat, and no padding byte changes.
    #[test]
    fn given_blur_and_all_background_mask_when_applied_then_the_picture_is_smoothed() {
        let squares = |x: usize, y: usize| {
            if (x / 16 + y / 16).is_multiple_of(2) {
                40
            } else {
                200
            }
        };
        let background = mask(256, 144, |_, _| 0);
        let mut effects = Effects::new();
        let original = Picture::new(640, 360, squares, 128, 128);
        let mut weak = Picture::new(640, 360, squares, 128, 128);
        effects
            .apply(blur(10), &mut weak.planes(), Some(&background))
            .unwrap();
        let mut strong = Picture::new(640, 360, squares, 128, 128);
        effects
            .apply(blur(100), &mut strong.planes(), Some(&background))
            .unwrap();
        let (v0, v1, v2) = (
            luma_variance(&original),
            luma_variance(&weak),
            luma_variance(&strong),
        );
        assert!(v1 < v0 && v2 < v1 / 4.0, "{v0} {v1} {v2}");
        assert!((i32::from(strong.luma(320, 180)) - 120).abs() <= 3);
        assert!(strong.padding_untouched());
        let mut flat = Picture::new(64, 36, |_, _| 77, 99, 155);
        effects
            .apply(blur(60), &mut flat.planes(), Some(&background))
            .unwrap();
        assert!(
            flat.y
                .chunks(flat.stride as usize)
                .all(|row| row[..64].iter().all(|&b| b == 77))
        );
        assert_eq!((flat.u_at(3, 3), flat.v_at(10, 5)), (99, 155));
    }

    /// GIVEN blur with a mask whose left half is the person
    /// WHEN it is applied
    /// THEN the left part stays sharp and the right part is blurred.
    #[test]
    fn given_half_mask_when_blurred_then_only_the_background_changes() {
        let mut picture = Picture::new(64, 32, checkerboard, 128, 128);
        let half = mask(64, 32, |x, _| if x < 32 { 255 } else { 0 });
        Effects::new()
            .apply(blur(100), &mut picture.planes(), Some(&half))
            .unwrap();
        for y in 0..32 {
            for x in 0..30 {
                assert_eq!(picture.luma(x, y), checkerboard(x, y), "{x} {y}");
            }
        }
        let right: Vec<u8> = (40..64).map(|x| picture.luma(x, 10)).collect();
        assert!(right.iter().all(|&v| v > 60 && v < 180), "{right:?}");
    }

    /// GIVEN each background picture and a mask of all background
    /// WHEN it is applied twice, and at two sizes
    /// THEN the picture is replaced with the drawn one, the same each time,
    /// different for each picture, and the drawn picture follows the size.
    #[test]
    fn given_background_pictures_when_applied_then_they_replace_the_background() {
        let background = mask(20, 10, |_, _| 0);
        let mut effects = Effects::new();
        let mut seen = Vec::new();
        for kind in BackgroundPicture::ALL {
            let mut first = Picture::new(48, 26, checkerboard, 60, 60);
            effects
                .apply(picture(kind), &mut first.planes(), Some(&background))
                .unwrap();
            let mut again = Picture::new(48, 26, |_, _| 3, 250, 250);
            effects
                .apply(picture(kind), &mut again.planes(), Some(&background))
                .unwrap();
            assert_eq!(first.y, again.y, "{kind:?}");
            assert_eq!((first.u.clone(), first.v.clone()), (again.u, again.v));
            assert!(first.padding_untouched());
            seen.push(first.y);
        }
        for (i, a) in seen.iter().enumerate() {
            for b in &seen[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // A new size draws again, at that size.
        let mut small = Picture::new(12, 6, checkerboard, 60, 60);
        effects
            .apply(
                picture(BackgroundPicture::Studio),
                &mut small.planes(),
                Some(&background),
            )
            .unwrap();
        assert!(small.padding_untouched());
        let centre = small.luma(6, 2);
        assert!(centre > small.luma(0, 5), "a lighter centre: {centre}");
    }

    /// GIVEN a picture background with a mask whose top half is the person
    /// WHEN it is applied
    /// THEN the top keeps the camera picture and the bottom shows the drawn
    /// picture.
    #[test]
    fn given_picture_and_half_mask_when_applied_then_the_person_is_kept() {
        let top = mask(16, 16, |_, y| if y < 8 { 255 } else { 0 });
        let mut picture_effect = Picture::new(32, 32, |_, _| 50, 128, 128);
        Effects::new()
            .apply(
                picture(BackgroundPicture::Dusk),
                &mut picture_effect.planes(),
                Some(&top),
            )
            .unwrap();
        assert!((0..14).all(|y| picture_effect.luma(5, y) == 50));
        assert!((18..32).all(|y| picture_effect.luma(5, y) != 50));
    }

    /// GIVEN a background effect and a colour filter together
    /// WHEN they are applied
    /// THEN the filter applies to the new background too.
    #[test]
    fn given_background_and_filter_when_applied_then_the_filter_comes_last() {
        let background = mask(8, 8, |_, _| 0);
        let mut picture_effect = Picture::new(16, 16, checkerboard, 128, 128);
        let effect = VideoEffect {
            background: Background::Picture(BackgroundPicture::Forest),
            filter: ColourFilter::Greyscale,
        };
        Effects::new()
            .apply(effect, &mut picture_effect.planes(), Some(&background))
            .unwrap();
        assert!(
            picture_effect.u[..8]
                .iter()
                .chain(&picture_effect.v[..8])
                .all(|&b| b == 128)
        );
    }

    /// GIVEN a mask of another size
    /// WHEN it is scaled to a picture
    /// THEN corners keep their values and the middle is between them.
    #[test]
    fn given_mask_of_another_size_when_scaled_then_values_are_interpolated() {
        let ramp = mask(2, 1, |x, _| if x == 0 { 0 } else { 255 });
        let (mut out, mut columns) = (Vec::new(), Vec::new());
        scale_mask(&ramp, 8, 3, &mut out, &mut columns);
        assert_eq!(out.len(), 24);
        assert_eq!((out[0], out[7]), (0, 255));
        assert!(out[3] > 0 && out[3] < 255 && out[3] < out[4]);
        assert_eq!(&out[..8], &out[16..]);
        // The kept columns of a wider picture do not change a narrower one.
        let mut narrow = Vec::new();
        scale_mask(&ramp, 4, 1, &mut narrow, &mut columns);
        assert_eq!(narrow.len(), 4);
        assert_eq!((narrow[0], narrow[3]), (0, 255));
        assert!(narrow[1] < narrow[2]);
        let mut same = Vec::new();
        scale_mask(&ramp, 2, 1, &mut same, &mut columns);
        assert_eq!(same, [0, 255]);
    }

    /// GIVEN mask sizes at and over the camera limits
    /// WHEN a mask is resized
    /// THEN only sizes inside the limits are taken.
    #[test]
    fn given_mask_sizes_when_resized_then_they_are_bounded() {
        let mut mask = Mask::default();
        assert!(mask.resize(640, 360) && mask.data_mut().len() == 230_400);
        assert!(mask.resize(256, 144) && mask.data_mut().len() == 256 * 144);
        assert!(!mask.resize(0, 10) && !mask.resize(641, 1) && !mask.resize(640, 361));
        assert_eq!(mask.data_mut().len(), 256 * 144);
    }

    /// GIVEN every blur strength
    /// WHEN the radius is taken for 640 and for 2 pixels wide
    /// THEN it runs from 2 to 20 at 640 and is never 0.
    #[test]
    fn given_blur_strengths_when_radius_taken_then_it_scales_with_width() {
        let radius = |percent, width| blur_radius(BlurStrength::new(percent).unwrap(), width);
        assert_eq!((radius(10, 640), radius(100, 640)), (2, 20));
        assert_eq!(radius(50, 320), 5);
        assert_eq!(radius(100, 2), 1);
    }

    /// A segmenter for the tests: a fixed mask, or a failure.
    struct Fixed(Option<u8>);

    impl Segmenter for Fixed {
        fn mask(&mut self, _: &Planes<'_>, out: &mut Mask) -> bool {
            let Some(value) = self.0 else {
                return false;
            };
            assert!(out.resize(4, 4));
            out.data_mut().fill(value);
            true
        }
    }

    /// GIVEN a feed with a shared effect
    /// WHEN the effect changes, or needs a mask with no segmenter or with
    /// one that fails
    /// THEN the feed follows the newest effect, and drops every picture
    /// that would go out without its background effect.
    #[test]
    fn given_camera_effects_when_processing_then_pictures_without_their_mask_are_dropped() {
        let shared = SharedEffect::new(VideoEffect::NONE);
        let mut none = CameraEffects::new(shared.clone(), None);
        let mut failing = CameraEffects::new(shared.clone(), Some(Box::new(Fixed(None))));
        let mut working = CameraEffects::new(shared.clone(), Some(Box::new(Fixed(Some(0)))));
        let run = |feed: &mut CameraEffects| {
            let mut picture = Picture::new(16, 8, checkerboard, 128, 128);
            let sent = feed.process(&mut picture.planes());
            (sent, picture.u_at(0, 0))
        };
        assert_eq!(run(&mut none), (true, 128));
        shared.store(filter(ColourFilter::Cool));
        assert_eq!(run(&mut none), (true, 138));
        shared.store(blur(30));
        assert!(!run(&mut none).0);
        assert!(!run(&mut failing).0);
        assert!(run(&mut working).0);
        assert_eq!(shared.load(), Some(blur(30)));
    }

    /// GIVEN every effect and each segmenter state
    /// WHEN the worker checks whether it can apply it
    /// THEN only blur and pictures need the segmenter, and every effect
    /// survives the shared value.
    #[test]
    fn given_effects_when_checked_then_background_effects_need_a_segmenter() {
        assert!(can_apply(filter(ColourFilter::Warm), false));
        assert!(can_apply(VideoEffect::NONE, false));
        assert!(!can_apply(blur(10), false) && can_apply(blur(10), true));
        assert!(!can_apply(picture(BackgroundPicture::Sea), false));
        let warm = filter(ColourFilter::Warm);
        assert_eq!(accept_effect(warm, blur(20), false), (warm, true));
        assert_eq!(accept_effect(warm, blur(20), true), (blur(20), false));
        assert_eq!(
            accept_effect(blur(20), VideoEffect::NONE, false),
            (VideoEffect::NONE, false)
        );
        let shared = SharedEffect::new(VideoEffect::NONE);
        for background in [
            Background::None,
            blur(70).background,
            picture(BackgroundPicture::Forest).background,
        ] {
            for filter in ColourFilter::ALL {
                let effect = VideoEffect { background, filter };
                shared.store(effect);
                assert_eq!(shared.load(), Some(effect));
            }
        }
    }

    /// GIVEN the drawn colours
    /// WHEN they are converted to video range YUV
    /// THEN black and white land on the range ends and grey is neutral.
    #[test]
    fn given_colours_when_converted_then_they_use_the_video_range() {
        assert_eq!(to_yuv([0.0, 0.0, 0.0]), [16, 128, 128]);
        assert_eq!(to_yuv([255.0, 255.0, 255.0]), [235, 128, 128]);
        assert_eq!(
            to_yuv([300.0, -5.0, 128.0])[0],
            to_yuv([255.0, 0.0, 128.0])[0]
        );
    }
}
