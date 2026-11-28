# One look, stalled in each phase

Made by `python3 tests/e13a/phases.py`. The deadline over a look is 10 s. A phase counts only if it was entered (the stand-in asked, the listener reached, the name server asked, a connection tried, or the look ran to its deadline). A CONTROL row puts back the fault its check exists for, and must fail for that reason.

| Phase | The component's look said | Must say | Entered, and how | As it must be | Its process left to itself |
|---|---|---|---|---|---|
| found (the baseline) | Found in 0.02 s | Found, within 2.0 s | stand-in: yes | yes | found after 0.3 s |
| the look's process was given descriptors | 0 1 2 3 | 0 1 2, and the one it reads them by | | yes | |
| name look-up stalls | NoAnswer in 8.024 s | NoAnswer, within 11.0 s | dns: yes | yes | exit 12 after 8.7 s |
| forward proxy stalls (HTTPS_PROXY) | NoAnswer in 5.026 s | NoAnswer, within 11.0 s | proxy: yes | yes | exit 12 after 5.6 s |
| connect never completes | NoAnswer in 5.01 s | NoAnswer, within 11.0 s | connect: yes | yes | exit 12 after 5.3 s |
| TLS never begins | NoAnswer in 5.025 s | NoAnswer, within 11.0 s | tls: yes | yes | exit 12 after 5.6 s |
| headers never come | NoAnswer in 8.126 s | NoAnswer, within 11.0 s | stand-in: yes | yes | exit 12 after 8.7 s |
| body never comes (200) | NoBackend in 8.11 s | NoBackend, within 11.0 s | stand-in: yes | yes | no backend after 8.7 s |
| a compressed 302 whose body never comes | NoBackend in 0.02 s | NoBackend, within 2.0 s | stand-in: yes | yes | no backend after 0.2 s |
| the look's process aborts (the shipped build's panic) | NoAnswer in 0.08 s | NoAnswer, within 2.0 s | spawned: yes | yes | exit 134 after 0.3 s |
| the look's process never ends | NoAnswer in 10.02 s | NoAnswer, within 11.0 s | deadline: yes | yes | still running at 40 s after 40.2 s |
| CONTROL: body opened before the status (W1's first version) | NoBackend in 8.148 s | NoBackend, within 2.0 s | stand-in: yes | **no** | no backend after 8.7 s |
| switched off: `--nexus-probe <address>` | the app started; nothing asked | the app starts, nothing asked | | yes | |
| switched off: `--nexus-probe-timed <address>` | the app started; nothing asked | the app starts, nothing asked | | yes | |
