# Rumpelpod Contribution Rules

Thanks for your interest in contributing to Rumpelpod. Contributions are
welcome from both NVIDIA-internal and external developers, subject to the
rules below.

#### Code of Conduct

Participation in the project is governed by the
[Code of Conduct](CODE_OF_CONDUCT.md). By contributing, you agree to abide
by it.

#### Issue Tracking

All bug reports, feature requests, and behavior-change proposals should
start with a GitHub issue. Templates live under `.github/ISSUE_TEMPLATE/`.
For larger changes, please open an issue and discuss the design before
submitting a pull request.

#### Pull Requests

The workflow is the standard fork-and-PR flow:

1. Fork the upstream repository.
2. Clone your fork and create a topic branch.
3. Make your changes and add or update tests for any new behavior.
4. Commit with a sign-off (`git commit -s`). See [Signing Your Work](#signing-your-work).
5. Push the branch to your fork and open a pull request against `master`.
6. Wait for review. At least one maintainer review is required before merge.

Keep pull requests focused. One feature or fix per pull request, with a
clear description of what changes and why.

#### Development Environment

You can use `rumpel` itself to spin up a development pod for working on
Rumpelpod. The `.devcontainer/` directory defines an image with the Rust
toolchain (and musl cross-compile targets), Docker, k3d, helm, and the
LLM agent CLIs preinstalled, ready for `cargo pipeline`.

The dev container needs the
[sysbox](https://github.com/nestybox/sysbox) runtime on the host.
Rumpelpod's tests run a Docker daemon and k3d clusters inside the
container, which sysbox supports without privileged mode. The default
`runc` runtime is not sufficient.

#### Testing

Run the full pipeline before submitting:

```bash
cargo pipeline
```

For a faster iteration loop on a subset of tests, pass a substring filter:

```bash
cargo pipeline enter_smoke_test
```

#### Signing Your Work

* We require that all contributors "sign-off" on their commits. This certifies that the contribution is your original work, or you have rights to submit it under the same license, or a compatible license.

  * Any contribution which contains commits that are not Signed-Off will not be accepted.

* To sign off on a commit you simply use the `--signoff` (or `-s`) option when committing your changes:
  ```bash
  $ git commit -s -m "Add cool feature."
  ```
  This will append the following to your commit message:
  ```
  Signed-off-by: Your Name <your@email.com>
  ```

* Full text of the DCO (https://developercertificate.org/):

  ```
    Developer Certificate of Origin
    Version 1.1

    Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

    Everyone is permitted to copy and distribute verbatim copies of this
    license document, but changing it is not allowed.


    Developer's Certificate of Origin 1.1

    By making a contribution to this project, I certify that:

    (a) The contribution was created in whole or in part by me and I
        have the right to submit it under the open source license
        indicated in the file; or

    (b) The contribution is based upon previous work that, to the best
        of my knowledge, is covered under an appropriate open source
        license and I have the right under that license to submit that
        work with modifications, whether created in whole or in part
        by me, under the same open source license (unless I am
        permitted to submit under a different license), as indicated
        in the file; or

    (c) The contribution was provided directly to me by some other
        person who certified (a), (b) or (c) and I have not modified
        it.

    (d) I understand and agree that this project and the contribution
        are public and that a record of the contribution (including all
        personal information I submit with it, including my sign-off) is
        maintained indefinitely and may be redistributed consistent with
        this project or the open source license(s) involved.
  ```
