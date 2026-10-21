# Evidence guidance

This area inherits `../AGENTS.md`.

- Reports are records of specific runs. Preserve their original commands, paths, hashes, outcomes, and limitations. Label later reruns or corrections explicitly.
- Record the command, completed exit status, source/lockfile identity, tool versions, OS, build type, inputs, and artifact locations needed to reproduce a material result.
- Separate observed facts from inference. A negative control must demonstrate that the measurement can detect the failure being tested. A crash or unsupported platform does not prove denial or successful containment.
- Update `verification.json` only from actual inspected results. Keep failures and unrun checks visible. Link new reports from `../README.md` and update relevant planning status when justified.
- Store raw output and disposable credentials under ignored `.local/`. Publish only reviewed, sanitized results. Use synthetic identifiers and data; inspect JSON and archives as well as prose.
- Validate changed JSON syntax and Markdown links. Do not regenerate expensive or live evidence merely to change documentation formatting. State when a report has not been rerun.
