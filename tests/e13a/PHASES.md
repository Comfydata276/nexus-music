# One look, stalled in each phase

Made by `python3 tests/e13a/phases.py`. The deadline over a look is 10 s. A CONTROL row puts back the fault its check exists for, and must fail.

| Phase | The component's look said | Must say | Within | Its process left to itself |
|---|---|---|---|---|
| found (the baseline) | Found in 0.02 s | Found, within 2.0 s | yes | found 5c0ffee05c0ffee05c0ffee05c0ffee0 after 0.2 s |
| name look-up stalls | NoAnswer in 8.023 s | NoAnswer, within 11.0 s | yes | (nothing) after 8.8 s |
| forward proxy stalls (HTTPS_PROXY) | NoAnswer in 5.156 s | NoAnswer, within 11.0 s | yes | (nothing) after 5.6 s |
| connect never completes | NoAnswer in 5.019 s | NoAnswer, within 11.0 s | yes | (nothing) after 5.3 s |
| TLS never begins | NoAnswer in 5.455 s | NoAnswer, within 11.0 s | yes | (nothing) after 5.6 s |
| headers never come | NoAnswer in 8.148 s | NoAnswer, within 11.0 s | yes | (nothing) after 8.7 s |
| body never comes (200) | NoBackend in 8.111 s | NoBackend, within 11.0 s | yes | none after 8.7 s |
| a compressed 302 whose body never comes | NoBackend in 0.02 s | NoBackend, within 2.0 s | yes | none after 0.2 s |
| the look's process aborts (the shipped build's panic) | NoAnswer in 0.08 s | NoAnswer, within 2.0 s | yes | (nothing) after 0.3 s |
| the look's process never ends | NoAnswer in 10.002 s | NoAnswer, within 11.0 s | yes | (nothing) after 40.2 s |
| CONTROL: body opened before the status (W1's first version) | NoBackend in 8.289 s | NoBackend, within 2.0 s | **no** | none after 8.7 s |
