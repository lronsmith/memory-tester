# memory-tester

A small Windows program that checks your computer's memory (RAM) for faults.
It shows the memory sticks installed in your PC, tests them while you wait,
and colours each stick by the result. Faulty RAM is a common cause of random
crashes, freezes and blue screens, and this program checks for it.

## Download

**[Download memory-tester.exe](https://github.com/lronsmith/memory-tester/releases/latest/download/memory-tester.exe)**

There is nothing to install. Save the file anywhere (your Downloads folder is
fine) and double-click it to run it. It works on Windows 10 and Windows 11
(64-bit).

## Download and "Windows protected your PC" warnings

Your web browser may warn that the file "isn't commonly downloaded" and hold
it back. In Microsoft Edge, click the **...** next to the download, then
**Keep**, then **Show more**, then **Keep anyway**. Other browsers have a
similar **Keep** option.

The first time you run it, Windows will probably show a blue box saying
**"Windows protected your PC"**. To continue, click **More info**, then
**Run anyway**.

Both warnings appear because the program is not code-signed. Signing costs
money every year and is normally done by companies; this is a small free
project, so it is not signed. Browsers and Windows warn about most small
programs downloaded from the internet. The warnings do not mean anything
wrong was found in the file.

## How to use it

1. Close your other programs. The test can only check memory that is not in
   use, so the more you close, the more it can test.
2. Open memory-tester and press **Start**.
3. Wait for the progress bar to finish. Leave the PC alone while it runs.

Each memory stick on screen is coloured:

- **Grey**: not tested yet.
- **Green**: passed. No fault was found.
- **Red**: this stick failed while it was the only stick installed. See the
  next section for what that means.
- **Amber**: a fault was found while more than one stick was installed, so the
  program cannot tell which of them is faulty. See the next section.

Once a stick has failed, the program keeps showing it as failed, even if a
later test passes, because a faulty stick can pass a short test. To clear
this, press **Forget saved results**.

## Finding the faulty stick

When several sticks are installed and the test finds a fault, it can only say
that one of them is faulty, not which one. To find it, test the sticks one at
a time:

1. Shut down the PC and unplug it from the wall.
2. Remove all memory sticks except one.
3. Start the PC, run memory-tester, and press Start.
4. Repeat for each stick in turn, putting each one in the same slot.

The program remembers each stick by its serial number, so after testing them
one by one it shows the result for all of them.

- If one stick is red and the others pass when tested alone in the same
  slot, the red stick is faulty and should be replaced.
- If every stick fails when tested alone, the cause is more likely the
  motherboard or its memory speed setting than the sticks.

**Tip:** if every stick fails, but only when the memory speed profile in the
motherboard settings (called XMP, DOCP or EXPO) is turned on, the sticks may
well be fine. The profile may simply be too fast for your system. Try turning
it off, or choosing a slower speed, and test again.

Some sticks cannot be remembered: those with no serial number, and those that
share a serial number with another installed stick. Write down the result
for those sticks yourself after each test.

## Limits

To be clear about what it cannot do:

- It cannot test the part of the memory that Windows itself is using while
  the program runs.
- A pass makes a memory fault unlikely, not impossible.
- A tester that starts the PC from a USB stick before Windows loads, such as
  [MemTest86](https://www.memtest86.com/), can test all of the memory and is
  more thorough. Use one of those if you want to be certain.
- With several sticks installed, it has no way to tell which stick a fault is
  on: Windows does not tell ordinary programs where in the physical memory
  their data lives, and most motherboards spread data across two sticks at
  once (dual channel).

## How it works

It reads the memory table provided by the PC's firmware to find the list of
installed sticks. It then takes free memory, locks it so Windows cannot move
it out to disk, and writes and checks a series of test patterns on all
processor cores at once.

It needs no administrator rights, uses no driver and does not use the
network. Nothing is installed: the only file it writes is a small results
file in `%LOCALAPPDATA%\memory-tester`.

## For developers

You need [Rust](https://www.rust-lang.org/tools/install) (stable). To build
the Windows exe you also need the Visual Studio C++ Build Tools, which Rust's
Windows (MSVC) toolchain uses for linking; the Rust installer on Windows
offers to install them.

Build:

```
cargo build --release
```

On Windows this produces `target\release\memory-tester.exe`. On macOS or
Linux the same command produces `target/release/memory-tester` (no `.exe`),
which is only useful for development.

Run the tests:

```
cargo test
```

The crate also builds and runs its tests on macOS and Linux for development;
the program itself is for Windows. Windows builds link the C runtime
statically (see `.cargo/config.toml`), so the exe runs on a clean Windows
install without the Visual C++ redistributable.

GitHub Actions (`.github/workflows/ci.yml`) checks formatting, runs clippy and
the tests, and builds the exe on Windows for every push and pull request.

To cut a release:

1. Set `version` in `Cargo.toml` (for example `0.2.0`), run `cargo build` so
   `Cargo.lock` picks it up, and commit both.
2. Push that commit to the branch on GitHub:

   ```
   git push
   ```

3. Tag the same commit with the version and a leading `v`, then push the tag:

   ```
   git tag v0.2.0
   git push origin v0.2.0
   ```

The workflow checks that the tag matches the version in `Cargo.toml`, runs
the tests, builds the exe, and publishes a GitHub Release with
`memory-tester.exe` and `memory-tester.exe.sha256` attached. Running it again
for a tag that already has a published release replaces those two files in
place. If the release exists only as a draft, the workflow stops and asks you
to publish or delete the draft first.

## Licence

MIT. See [LICENSE](LICENSE).
