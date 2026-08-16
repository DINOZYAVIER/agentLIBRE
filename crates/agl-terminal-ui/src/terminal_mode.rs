use super::*;

pub(super) fn restore_physical_terminal() {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\x1b[0m");
    let _ = execute!(stdout, DisableBracketedPaste, Show);
    let _ = stdout.flush();
    let _ = disable_raw_mode();
}

pub(super) struct TuiTerminalMode {
    active: Arc<AtomicBool>,
}

impl TuiTerminalMode {
    pub(super) fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error).context("failed to enable bracketed paste");
        }
        let active = Arc::new(AtomicBool::new(true));
        let hook_active = Arc::clone(&active);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            if hook_active.swap(false, Ordering::AcqRel) {
                restore_physical_terminal();
            }
            previous(panic);
        }));
        Ok(Self { active })
    }

    pub(super) fn suspend(&mut self) {
        if self.active.swap(false, Ordering::AcqRel) {
            restore_physical_terminal();
        }
    }

    pub(super) fn resume(&mut self) -> Result<()> {
        enable_raw_mode().context("failed to restore terminal raw mode after SIGCONT")?;
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste, Show) {
            restore_physical_terminal();
            return Err(error).context("failed to restore terminal modes after SIGCONT");
        }
        self.active.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for TuiTerminalMode {
    fn drop(&mut self) {
        if self.active.swap(false, Ordering::AcqRel) {
            restore_physical_terminal();
        }
    }
}
