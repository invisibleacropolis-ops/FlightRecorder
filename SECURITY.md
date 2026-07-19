# Security policy

## Supported release

`0.2.0-preview.1` is an unsigned Windows x64 preview. It is not yet a stable production release.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this repository. Do not include real recordings, decrypted keyboard content, encryption keys, or personal snapshots. Include versions, hashes, redacted logs, and reproduction steps where possible.

The reviewer binds only to an ephemeral `127.0.0.1` port and requires a random per-launch session cookie. The IPC endpoint rejects clients from a different Windows logon session. Processes running as the same interactive Windows user remain inside the local trust boundary.
