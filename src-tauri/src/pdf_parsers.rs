use crate::app_runtime::AppHandle;
use crate::{
    local_jobs::LocalJobRegistry,
    model_setup_paths::docling_runtime_setup_dir,
    pdf_runtime_install::{
        download_pinned_uv, warmup_pdf_path, write_pdf_runtime_files,
    },
    pdf_runtime_processes::uv_command_path,
    pdf_runtimes::{
        docling_helper_path_hint, docling_python_command, docling_runtime_status,
        pymupdf4llm_helper_path_hint, pymupdf4llm_python_command,
        write_docling_runtime_setup_manifest, DoclingRuntimeStatus,
    },
    process_utils::{
        command_output_with_timeout, command_output_with_timeout_and_cancel,
        parse_json_command_output,
    },
    storage::{create_dir_all, display_path},
};
use std::{
    path::Path,
    sync::Mutex,
    time::Duration,
};

/// Set-up on a machine that has nothing yet downloads Python, Docling with
/// PyTorch, and Docling's models. Thirty minutes covers a slow connection.
const DOCLING_SETUP_STEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Where a running Docling set-up is, for the status the settings page reads.
#[derive(Debug, Clone, Default)]
pub(crate) struct DoclingSetupProgress {
    pub(crate) running: bool,
    pub(crate) step: Option<String>,
    pub(crate) error: Option<String>,
}

static DOCLING_SETUP: Mutex<DoclingSetupProgress> = Mutex::new(DoclingSetupProgress {
    running: false,
    step: None,
    error: None,
});

fn docling_setup() -> std::sync::MutexGuard<'static, DoclingSetupProgress> {
    DOCLING_SETUP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn docling_setup_progress() -> DoclingSetupProgress {
    docling_setup().clone()
}

fn set_docling_setup_step(step: &str) {
    docling_setup().step = Some(step.to_string());
}

fn run_docling_runtime_probe(timeout: Duration) -> Result<serde_json::Value, String> {
    let probe = r#"
import importlib.metadata
import json
import sys
from docling.document_converter import DocumentConverter
from docling.datamodel.base_models import DocumentStream

print(json.dumps({
    "doclingVersion": importlib.metadata.version("docling"),
    "pythonExecutable": sys.executable,
}))
"#;
    let mut command = docling_python_command();
    command.arg("-c").arg(probe);
    let output = command_output_with_timeout(&mut command, "Docling runtime probe", timeout)?;
    parse_json_command_output(output, "Docling runtime probe")
}

/// Every step of set-up, in order. Each step can be repeated: a set-up that
/// failed half way is started again from the top and skips what is done.
fn run_docling_setup(app: &AppHandle) -> Result<(), String> {
    set_docling_setup_step("Writing the helper scripts");
    write_pdf_runtime_files()?;
    if std::env::var("MNEMOSYNE_DOCLING_PYTHON").is_err() && uv_command_path().is_none() {
        set_docling_setup_step("Downloading uv (about 20 MB)");
        download_pinned_uv()?;
    }
    set_docling_setup_step("Downloading Python and Docling (this is the long step)");
    let probe = run_docling_runtime_probe(DOCLING_SETUP_STEP_TIMEOUT)?;
    set_docling_setup_step("Downloading Docling's models and converting a test page");
    run_docling_helper(
        app,
        &warmup_pdf_path(),
        "warmup.pdf",
        None,
        DOCLING_SETUP_STEP_TIMEOUT,
        None,
        true,
    )?;
    write_docling_runtime_setup_manifest(app, &probe)?;
    Ok(())
}

/// Starts set-up in the background and answers at once; the status says
/// "preparing" with the current step until it ends. A second request while
/// one runs starts nothing.
pub(crate) fn prepare_docling_runtime_impl(
    app: &AppHandle,
) -> Result<DoclingRuntimeStatus, String> {
    let status = docling_runtime_status(app)?;
    if status.available || !status.supported {
        return Ok(status);
    }
    {
        let mut setup = docling_setup();
        if setup.running {
            drop(setup);
            return docling_runtime_status(app);
        }
        *setup = DoclingSetupProgress {
            running: true,
            step: Some("Starting".to_string()),
            error: None,
        };
    }
    let worker_app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("docling-setup".to_string())
        .spawn(move || {
            // A panic must not leave the status at "preparing" for ever.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_docling_setup(&worker_app)
            }))
            .unwrap_or_else(|_| Err("Docling set-up stopped unexpectedly".to_string()));
            let mut setup = docling_setup();
            setup.running = false;
            setup.step = None;
            setup.error = result.err();
        });
    if let Err(error) = spawned {
        let mut setup = docling_setup();
        setup.running = false;
        setup.step = None;
        setup.error = Some(format!("start Docling set-up: {error}"));
    }
    docling_runtime_status(app)
}

pub(crate) fn run_docling_pdf_to_markdown(
    app: &AppHandle,
    input_path: &Path,
    filename: &str,
    title: Option<&str>,
    timeout: Duration,
    cancel: Option<(&LocalJobRegistry, &str)>,
) -> Result<serde_json::Value, String> {
    run_docling_helper(app, input_path, filename, title, timeout, cancel, false)
}

fn run_docling_helper(
    app: &AppHandle,
    input_path: &Path,
    filename: &str,
    title: Option<&str>,
    timeout: Duration,
    cancel: Option<(&LocalJobRegistry, &str)>,
    force_ocr: bool,
) -> Result<serde_json::Value, String> {
    write_pdf_runtime_files()?;
    let helper_path = docling_helper_path_hint();
    let helper = Path::new(&helper_path);
    if !helper.is_file() {
        return Err(format!(
            "Docling helper script is missing: {}",
            display_path(helper)
        ));
    }
    let rapidocr_cache = docling_runtime_setup_dir(app)?.join("rapidocr-models");
    create_dir_all(&rapidocr_cache)?;
    let mut command = docling_python_command();
    command
        .arg(helper)
        .arg("--input")
        .arg(input_path)
        .arg("--filename")
        .arg(filename)
        .arg("--rapidocr-cache")
        .arg(&rapidocr_cache);
    if force_ocr {
        command.arg("--force-ocr");
    }
    if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
        command.arg("--title").arg(title);
    }
    // With a job, a cancel reaches the child (it used to run on to its
    // timeout after the job was already marked cancelled).
    let output = match cancel {
        Some((jobs, job_id)) => command_output_with_timeout_and_cancel(
            &mut command,
            "Docling PDF conversion",
            timeout,
            jobs,
            job_id,
        )?,
        None => command_output_with_timeout(&mut command, "Docling PDF conversion", timeout)?,
    };
    parse_json_command_output(output, "Docling PDF conversion")
}

pub(crate) fn run_pymupdf4llm_pdf_to_markdown(
    input_path: &Path,
    filename: &str,
    title: Option<&str>,
    timeout: Duration,
    jobs: &LocalJobRegistry,
    job_id: &str,
) -> Result<serde_json::Value, String> {
    run_pymupdf4llm_pdf_to_markdown_optional_cancel(
        input_path,
        filename,
        title,
        timeout,
        Some((jobs, job_id)),
    )
}

pub(crate) fn run_pymupdf4llm_pdf_to_markdown_optional_cancel(
    input_path: &Path,
    filename: &str,
    title: Option<&str>,
    timeout: Duration,
    cancel: Option<(&LocalJobRegistry, &str)>,
) -> Result<serde_json::Value, String> {
    write_pdf_runtime_files()?;
    let helper_path = pymupdf4llm_helper_path_hint();
    let helper = Path::new(&helper_path);
    if !helper.is_file() {
        return Err(format!(
            "PyMuPDF4LLM helper script is missing: {}",
            display_path(helper)
        ));
    }
    let mut command = pymupdf4llm_python_command();
    command
        .arg(helper)
        .arg("--input")
        .arg(input_path)
        .arg("--filename")
        .arg(filename);
    if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
        command.arg("--title").arg(title);
    }
    let output = match cancel {
        Some((jobs, job_id)) => command_output_with_timeout_and_cancel(
            &mut command,
            "PyMuPDF4LLM PDF conversion",
            timeout,
            jobs,
            job_id,
        )?,
        None => command_output_with_timeout(&mut command, "PyMuPDF4LLM PDF conversion", timeout)?,
    };
    parse_json_command_output(output, "PyMuPDF4LLM PDF conversion")
}
