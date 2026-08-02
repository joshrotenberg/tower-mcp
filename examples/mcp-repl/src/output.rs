//! Prompt-safe output for asynchronous server diagnostics.
//!
//! While reedline owns the terminal, output goes through its external printer
//! so the active input is cleared and redrawn. At startup, during a command,
//! and in `--exec` mode, diagnostics stay on stderr.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use reedline::ExternalPrinter;

/// Shared destination for output that can arrive independently of a command.
#[derive(Clone)]
pub struct AsyncOutput {
    at_prompt: Arc<AtomicBool>,
    printer: Option<ExternalPrinter<String>>,
}

impl AsyncOutput {
    /// Build a sink, with a reedline printer only for interactive sessions.
    pub fn new(at_prompt: Arc<AtomicBool>, interactive: bool) -> Self {
        Self {
            at_prompt,
            printer: interactive.then(|| ExternalPrinter::new(1024)),
        }
    }

    /// Clone the printer that the editor must install, when interactive.
    pub fn external_printer(&self) -> Option<ExternalPrinter<String>> {
        self.printer.clone()
    }

    /// Print one logical line without disturbing an active prompt.
    pub fn line(&self, line: impl Into<String>) {
        let line = line.into();
        if self.at_prompt.load(Ordering::SeqCst)
            && let Some(printer) = &self.printer
            && printer.print(line.clone()).is_ok()
        {
            return;
        }
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_prompt_uses_reedline_external_printer() {
        let at_prompt = Arc::new(AtomicBool::new(true));
        let output = AsyncOutput::new(at_prompt, true);

        output.line("server diagnostic");

        assert_eq!(
            output.external_printer().unwrap().get_line().as_deref(),
            Some("server diagnostic")
        );
    }

    #[test]
    fn non_interactive_output_has_no_printer() {
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(false)), false);
        assert!(output.external_printer().is_none());
    }
}
