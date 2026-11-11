// Copyright © Advanced Micro Devices, Inc., or its affiliates.
//
// SPDX-License-Identifier: MIT

//! Steps for `rocm diagnose --symptom-file` (diagnose-59 to diagnose-61): a
//! pasted error handed over on standard input or in a file instead of as a
//! shell argument.

use std::path::{Path, PathBuf};
use std::process::Command;

use cucumber::{given, then, when};

use crate::E2eWorld;
use crate::e2e::tui_driver::{TuiSession, default_timeout};

/// A pasted error that no shell passes through intact as an argument: it
/// starts with `-`, carries double and single quotes, a `$(...)` that bash and
/// PowerShell would run, and a cmd.exe `&` chain. Both of the commands it
/// smuggles in would write [`SHELL_RAN_MARKER`] into the directory the shell
/// runs in. The keyword it carries scores a catalog entry on every platform
/// (see `KNOWN_SYMPTOM` in `diagnose_steps.rs`), so the diagnosis it gets is a
/// real one. ASCII only: Windows PowerShell 5.1 pipes text to a native command
/// as ASCII by default, and what this checks is the shell's parsing, not its
/// encoding.
const HOSTILE_PASTE: &str = "-v \"HSA_STATUS_ERROR_INVALID_ISA\" isn't it? \
     $(echo ran > shell-ran-the-paste) & echo ran > shell-ran-the-paste";

/// The file either smuggled-in command would create.
const SHELL_RAN_MARKER: &str = "shell-ran-the-paste";

/// The args every piped run hands `rocm`, for the coverage report.
const PIPED_ARGS: [&str; 4] = ["diagnose", "--symptom-file", "-", "--json"];

/// The terminal scenario asks for one cause, as does the argument form it is
/// compared with. The report is still taller than the pseudo-terminal's fixed
/// screen; only its closing line is compared, and that stays in view.
const TERMINAL_ARGS: [&str; 5] = ["diagnose", "--symptom-file", "-", "--top", "1"];

/// One `rocm diagnose --symptom-file` run: which shell piped it (or which file
/// it was pointed at), and what came back.
#[derive(Debug)]
pub struct SymptomFileRun {
    pub label: String,
    pub path: Option<PathBuf>,
    pub rc: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The scenario's own directory for the paste and anything a shell writes.
fn paste_dir(world: &E2eWorld) -> PathBuf {
    let dir = world
        .isolated_root
        .as_ref()
        .expect("no isolated root")
        .path()
        .join("paste");
    std::fs::create_dir_all(&dir).expect("failed to create the paste directory");
    dir
}

/// Run one shell pipeline in the paste directory, with the scenario's
/// isolated environment, and record it as the `rocm` invocation it wraps.
fn run_piped(world: &E2eWorld, label: &str, mut cmd: Command) -> SymptomFileRun {
    cmd.current_dir(paste_dir(world));
    world.isolate_cmd(&mut cmd);
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to start {label}: {e}"));
    let rc = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    crate::record_command(world.current_scenario.as_deref(), &PIPED_ARGS, rc, &stdout);
    SymptomFileRun {
        label: label.to_owned(),
        path: None,
        rc,
        stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The pipelines a user on this platform would type: the paste comes from a
/// file through the shell's own pipe, so the shell parses only the command.
#[cfg(not(windows))]
fn platform_pipelines(rocm: &str, paste: &Path) -> Vec<(&'static str, Command)> {
    let mut bash = Command::new("bash");
    bash.arg("-c")
        .arg(r#"cat -- "$1" | "$0" diagnose --symptom-file - --json"#)
        .arg(rocm)
        .arg(paste);
    vec![("bash", bash)]
}

#[cfg(windows)]
fn platform_pipelines(rocm: &str, paste: &Path) -> Vec<(&'static str, Command)> {
    use std::os::windows::process::CommandExt as _;

    let paste = paste.display().to_string();
    // `/s` strips exactly the outer pair of quotes, leaving each path quoted.
    let mut cmd = Command::new("cmd.exe");
    cmd.raw_arg(format!(
        r#"/d /s /c "type "{paste}" | "{rocm}" diagnose --symptom-file - --json""#
    ));
    // PowerShell's pipe to a native command appends a line break, which is the
    // one `--symptom-file` removes. `exit $LASTEXITCODE` hands back rocm's own
    // exit code rather than PowerShell's success flag.
    let single = |text: &str| text.replace('\'', "''");
    let mut powershell = Command::new("powershell.exe");
    powershell.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!(
            "Get-Content -Raw -LiteralPath '{}' | & '{}' diagnose --symptom-file - --json; \
             exit $LASTEXITCODE",
            single(&paste),
            single(rocm)
        ),
    ]);
    vec![("cmd.exe", cmd), ("Windows PowerShell", powershell)]
}

/// The diagnosis of `paste` handed over as one argument with no shell in
/// between: the reference every other route must agree with.
fn diagnose_as_argument(world: &E2eWorld, paste: &str) -> serde_json::Value {
    let flag = format!("--symptom={paste}");
    let stdout = crate::run_rocm_ok(world, &["diagnose", &flag, "--json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("diagnose --json did not emit JSON ({e}):\n{stdout}"));
    assert_eq!(
        report.get("has_match").and_then(serde_json::Value::as_bool),
        Some(true),
        "the paste must be recognised when passed as an argument, or the comparison \
         would be between two empty diagnoses:\n{stdout}"
    );
    report
}

/// What two diagnoses must agree on: every cause, its score and its evidence.
fn causes(report: &serde_json::Value) -> Vec<(String, i64, serde_json::Value)> {
    report
        .get("matched")
        .and_then(serde_json::Value::as_array)
        .expect("the report carries a matched list")
        .iter()
        .map(|cause| {
            (
                cause["id"].as_str().unwrap_or_default().to_owned(),
                cause["score"].as_i64().unwrap_or_default(),
                cause["evidence"].clone(),
            )
        })
        .collect()
}

// ── Given ──────────────────────────────────────────────────────────

#[given("a pasted error containing quotes, a command substitution and a leading minus sign")]
async fn a_hostile_paste(world: &mut E2eWorld) {
    // Saved without a trailing line break, as a clipboard holds it.
    std::fs::write(paste_dir(world).join("paste.txt"), HOSTILE_PASTE)
        .expect("failed to save the paste");
    world.model_name = Some(HOSTILE_PASTE.to_owned());
}

#[given("an error file that does not exist and one that is not text")]
async fn unusable_error_files(world: &mut E2eWorld) {
    let dir = paste_dir(world);
    // Starts with a letter, so it is not mistaken for a UTF-16 byte-order mark.
    std::fs::write(dir.join("capture.bin"), [b'H', 0xFF, 0xFE, 0x00, 0x81])
        .expect("failed to write the binary file");
    world.symptom_file_runs = vec![
        SymptomFileRun {
            label: "missing".into(),
            path: Some(dir.join("no-such-error.txt")),
            rc: -1,
            stdout: String::new(),
            stderr: String::new(),
        },
        SymptomFileRun {
            label: "not text".into(),
            path: Some(dir.join("capture.bin")),
            rc: -1,
            stdout: String::new(),
            stderr: String::new(),
        },
    ];
}

// ── When ───────────────────────────────────────────────────────────

#[when("the user pipes it to the diagnosis instead of passing it as an argument")]
async fn pipe_the_paste(world: &mut E2eWorld) {
    let paste = paste_dir(world).join("paste.txt");
    let rocm = crate::rocm_binary();
    let runs: Vec<SymptomFileRun> = platform_pipelines(&rocm, &paste)
        .into_iter()
        .map(|(label, cmd)| run_piped(world, label, cmd))
        .collect();
    world.symptom_file_runs = runs;
}

#[when("the user asks to paste the error at the terminal, pastes it and ends it as told")]
async fn paste_at_the_terminal(world: &mut E2eWorld) {
    let paste = world.model_name.clone().expect("no paste set");
    let mut session = TuiSession::spawn(world, &TERMINAL_ARGS)
        .unwrap_or_else(|e| panic!("failed to start the diagnosis: {e}"));
    // Waits for the hint's last words too: it is wider than the terminal, so
    // its first row can be on screen before the rest has been drawn.
    session
        .wait_for_screen_where(
            "the CLI asked for the paste",
            |screen| screen.contains("Paste the error text") && screen.contains("Windows)."),
            default_timeout(),
        )
        .await
        .unwrap_or_else(|e| panic!("the CLI never asked for the paste: {e}"));
    // The screen while the CLI waits for the paste. The report printed after
    // it is taller than the terminal, which has no scrollback, so by the end
    // the hint has scrolled away; the Then step reads it from here.
    let asked = session.screen_text();
    // What the hint tells the user to press: Ctrl-D at the start of a line,
    // or on Windows Ctrl-Z then Enter.
    let end = if cfg!(windows) { "\u{1a}\r" } else { "\u{4}" };
    session
        .send(&format!("{paste}\r{end}"))
        .unwrap_or_else(|e| panic!("failed to type the paste: {e}"));
    let rc = session
        .wait_for_exit_code(default_timeout())
        .await
        .unwrap_or_else(|e| panic!("the CLI did not finish after the paste was ended: {e}"));
    world.cli_outputs = Some(vec![asked]);
    world.cli_output = Some(session.screen_text());
    world.cli_rc = Some(rc);
    world.tui = Some(session);
}

#[when("the user asks the CLI to diagnose the error in each file")]
async fn diagnose_each_file(world: &mut E2eWorld) {
    let mut runs = std::mem::take(&mut world.symptom_file_runs);
    for run in &mut runs {
        let path = run
            .path
            .as_ref()
            .expect("no file set")
            .display()
            .to_string();
        let (stdout, stderr, rc) =
            crate::run_rocm(world, &["diagnose", "--symptom-file", &path, "--json"]);
        run.rc = rc;
        run.stdout = stdout;
        run.stderr = stderr;
    }
    world.symptom_file_runs = runs;
}

// ── Then ───────────────────────────────────────────────────────────

#[then(
    "the diagnosis matches the same failure modes as the same text passed safely as an argument"
)]
async fn piped_matches_argument(world: &mut E2eWorld) {
    let paste = world.model_name.clone().expect("no paste set");
    let expected = causes(&diagnose_as_argument(world, &paste));
    assert!(
        !world.symptom_file_runs.is_empty(),
        "no shell piped the paste"
    );
    for run in &world.symptom_file_runs {
        assert_eq!(
            run.rc, 0,
            "{} pipe: diagnose should exit 0\nstdout:\n{}\nstderr:\n{}",
            run.label, run.stdout, run.stderr
        );
        let report: serde_json::Value = serde_json::from_str(&run.stdout).unwrap_or_else(|e| {
            panic!(
                "{} pipe: diagnose --json did not emit JSON ({e}):\n{}\nstderr:\n{}",
                run.label, run.stdout, run.stderr
            )
        });
        assert_eq!(
            causes(&report),
            expected,
            "{} pipe: the piped paste was diagnosed differently from the same text \
             passed as an argument",
            run.label
        );
    }
}

#[then("no part of the paste was run by the shell or read as a flag")]
async fn paste_was_not_run(world: &mut E2eWorld) {
    let marker = paste_dir(world).join(SHELL_RAN_MARKER);
    assert!(
        !marker.exists(),
        "a shell ran a command smuggled in the paste: {} exists",
        marker.display()
    );
    for run in &world.symptom_file_runs {
        // A paste read as arguments would have been refused by the parser
        // (a usage error, exit 2) for its leading `-v` and stray words.
        assert!(
            !run.stderr.contains("unexpected argument") && run.rc == 0,
            "{} pipe: part of the paste reached the command line (exit {}):\n{}",
            run.label,
            run.rc,
            run.stderr
        );
    }
}

#[then("the CLI told the user how to end the paste")]
async fn told_how_to_end(world: &mut E2eWorld) {
    let asked = world
        .cli_outputs
        .as_ref()
        .and_then(|screens| screens.first())
        .expect("no screen recorded while the CLI waited for the paste");
    // The hint's own words, on both platforms: the end-of-input key it names is
    // the one the When step pressed, and the CLI finished because of it. It is
    // wider than the terminal, so where it wraps is ignored.
    let squeeze = |text: &str| text.split_whitespace().collect::<String>();
    assert!(
        squeeze(asked).contains(&squeeze(
            "Paste the error text, then end it with Ctrl-D at the start of a line \
             (Ctrl-Z then Enter on Windows)."
        )),
        "the terminal never showed how to end the paste:\n{asked}"
    );
    let screen = world.cli_output.as_deref().expect("no terminal output");
    assert_eq!(
        world.cli_rc,
        Some(0),
        "the paste ended and diagnose exits 0:\n{screen}"
    );
}

#[then("the pasted error is diagnosed the same as the same text passed safely as an argument")]
async fn terminal_paste_matches_argument(world: &mut E2eWorld) {
    let paste = world.model_name.clone().expect("no paste set");
    // Establishes that the paste is recognised at all, so the closing line
    // compared below names a cause rather than being absent from both.
    diagnose_as_argument(world, &paste);
    let flag = format!("--symptom={paste}");
    let reference = crate::run_rocm_ok(world, &["diagnose", &flag, "--top", "1"]);
    // The report's closing line names the cause to fix (`Next step: run
    // `rocm fix <id>`` or, below high confidence, the `When you are ready`
    // line). It is the last thing printed, so it is on screen however much of
    // the report above it scrolled away; compared without the terminal's line
    // wrapping.
    let closing = reference
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_else(|| panic!("the argument form printed nothing:\n{reference}"));
    let squeeze = |text: &str| text.split_whitespace().collect::<String>();
    let screen = world.cli_output.as_deref().expect("no terminal output");
    assert!(
        squeeze(screen).contains(&squeeze(closing)),
        "the terminal paste did not end with the same verdict as the same text passed as \
         an argument ({closing:?}):\n{screen}"
    );
}

#[then("each is refused as a usage error that names the file and says why")]
async fn each_file_refused(world: &mut E2eWorld) {
    assert_eq!(
        world.symptom_file_runs.len(),
        2,
        "both files must have been tried"
    );
    for run in &world.symptom_file_runs {
        let path = run
            .path
            .as_ref()
            .expect("no file set")
            .display()
            .to_string();
        let why = if run.label == "missing" {
            "could not read the symptom from"
        } else {
            "is not valid UTF-8"
        };
        assert_eq!(
            run.rc, 2,
            "{} file: a usage error exits 2\nstderr:\n{}",
            run.label, run.stderr
        );
        for needed in [path.as_str(), why, "Usage: rocm diagnose"] {
            assert!(
                run.stderr.contains(needed),
                "{} file: the refusal must say {needed:?}:\n{}",
                run.label,
                run.stderr
            );
        }
        assert!(
            !run.stderr.contains("panicked"),
            "{} file: refused by a panic:\n{}",
            run.label,
            run.stderr
        );
    }
}

#[then("nothing is diagnosed")]
async fn nothing_diagnosed(world: &mut E2eWorld) {
    for run in &world.symptom_file_runs {
        assert!(
            run.stdout.trim().is_empty(),
            "{} file: a refused file must not produce a diagnosis:\n{}",
            run.label,
            run.stdout
        );
    }
}
