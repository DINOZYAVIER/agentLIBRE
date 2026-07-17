Attach this local terminal to a live daemon-owned execution. Retained output is
replayed after `--after`, followed by live output. One writable attachment is
allowed; `--read-only` does not acquire the input lease. Press Ctrl-] to detach
without killing the target. Terminal settings are restored on detach, target
exit, interrupt, protocol failure, or unwind.
