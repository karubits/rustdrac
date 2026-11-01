# Contributing to Rust Drac

Thanks for helping out! Rust Drac is a small project, and every bug report, idea and pull
request makes it better.

## Reporting bugs and requesting features

Open an [issue](https://github.com/karubits/rustdrac/issues). For bugs, please include:
- the Rust Drac version (**Menu → About Rust Drac**, or `rustdrac --version`);
- the BMC (iDRAC 8, iDRAC 9 or Supermicro X11) and its firmware version, and the server model;
- your distro, and whether you use the AppImage or a source build;
- what you did, what you expected and what happened. Output from `RUSTDRAC_DEBUG=1 rustdrac …`
  often helps.

**Never paste passwords, session cookies or internal hostnames you don't want public.**

## Pull requests

1. Fork the repository and create a branch from `master`.
2. Build it: see [Build from source](README.md#build-from-source) for the GTK / WebKitGTK
   packages.
3. Make your change. Keep to the style of the surrounding code, and add a test when the
   logic can be tested without a real BMC.
4. Run the same checks as CI:

   ```sh
   cargo fmt --all
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

5. Add a line under today's `## YYYY-MM-DD` heading in [CHANGELOG.md](CHANGELOG.md)
   (Keep a Changelog categories: Added, Changed, Fixed, …).
6. Open a pull request that describes the change and how you tested it. Mention which
   BMC firmware versions you tried it on.

## Testing against real hardware

Most of Rust Drac talks to a real BMC, so a few tests are `#[ignore]`d and need one:

```sh
RUSTDRAC_TEST_HOST=… RUSTDRAC_TEST_USER=… RUSTDRAC_TEST_PASSWORD=… \
  cargo test -- --ignored export_from_a_real_bmc --nocapture
```

The [Debugging](README.md#debugging) section lists environment variables that help when
you work on the console or the UI.

## License

By contributing you agree that your work is released under the [MIT License](LICENSE).
