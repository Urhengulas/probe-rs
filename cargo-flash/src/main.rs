use anyhow::{anyhow, Context, Result};
use colored::*;
use diagnostics::{handle_flash_error, Diagnostic};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::{
    env,
    fs::File,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::Instant,
};
use std::{fmt::Write, panic, sync::Mutex};
use structopt::StructOpt;

use probe_rs::{
    config::TargetSelector,
    flashing::{download_elf, FlashLoader, FlashProgress, ProgressEvent},
    DebugProbeSelector, FakeProbe, Probe, Session, Target, WireProtocol,
};

#[cfg(feature = "sentry")]
use probe_rs_cli_util::logging::{ask_to_log_crash, capture_anyhow, capture_panic};
use probe_rs_cli_util::{
    argument_handling, build_artifact, logging, logging::Metadata, read_metadata,
};

mod diagnostics;

lazy_static::lazy_static! {
    static ref METADATA: Arc<Mutex<Metadata>> = Arc::new(Mutex::new(Metadata {
        release: env!("CARGO_PKG_VERSION").to_string(),
        chip: None,
        probe: None,
        speed: None,
        commit: git_version::git_version!(fallback = "crates.io").to_string(),
    }));
}

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(name = "chip", long = "chip")]
    chip: Option<String>,
    #[structopt(name = "chip description file path", long = "chip-description-path")]
    chip_description_path: Option<String>,
    // TODO: enable once the plugin architecture is here.
    // #[structopt(name = "nrf-recover", long = "nrf-recover")]
    // nrf_recover: bool,
    #[structopt(name = "list-chips", long = "list-chips")]
    list_chips: bool,
    #[structopt(
        name = "list-probes",
        long = "list-probes",
        help = "Lists all the connected probes that can be seen.\n\
        If udev rules or permissions are wrong, some probes might not be listed."
    )]
    list_probes: bool,
    #[structopt(name = "disable-progressbars", long = "disable-progressbars")]
    disable_progressbars: bool,
    #[structopt(name = "protocol", long = "protocol", default_value = "swd")]
    protocol: WireProtocol,
    #[structopt(
        long = "probe",
        help = "Use this flag to select a specific probe in the list.\n\
        Use '--probe VID:PID' or '--probe VID:PID:Serial' if you have more than one probe with the same VID:PID."
    )]
    probe_selector: Option<DebugProbeSelector>,
    #[structopt(
        long = "connect-under-reset",
        help = "Use this flag to assert the nreset & ntrst pins during attaching the probe to the chip."
    )]
    connect_under_reset: bool,
    #[structopt(
        name = "reset-halt",
        long = "reset-halt",
        help = "Use this flag to reset and halt (instead of just a reset) the attached core after flashing the target."
    )]
    reset_halt: bool,
    #[structopt(
        name = "level",
        long = "log",
        help = "Use this flag to set the log level.\n\
        Default is `warning`. Possible choices are [error, warning, info, debug, trace]."
    )]
    log: Option<log::Level>,
    #[structopt(name = "speed", long = "speed", help = "The protocol speed in kHz.")]
    speed: Option<u32>,
    #[structopt(
        name = "restore-unwritten",
        long = "restore-unwritten",
        help = "Enable this flag to restore all bytes erased in the sector erase but not overwritten by any page."
    )]
    restore_unwritten: bool,
    #[structopt(
        name = "filename",
        long = "flash-layout",
        help = "Requests the flash builder to output the layout into the given file in SVG format."
    )]
    flash_layout_output_path: Option<String>,
    #[structopt(
        name = "elf file",
        long = "elf",
        help = "The path to the ELF file to be flashed."
    )]
    elf: Option<String>,
    #[structopt(
        name = "directory",
        long = "work-dir",
        help = "The work directory from which cargo-flash should operate from."
    )]
    work_dir: Option<String>,

    #[structopt(long = "dry-run")]
    dry_run: bool,

    // `cargo build` arguments
    #[structopt(name = "binary", long = "bin")]
    bin: Option<String>,
    #[structopt(name = "example", long = "example")]
    example: Option<String>,
    #[structopt(name = "package", short = "p", long = "package")]
    package: Option<String>,
    #[structopt(name = "release", long = "release")]
    release: bool,
    #[structopt(name = "target", long = "target")]
    target: Option<String>,
    #[structopt(name = "PATH", long = "manifest-path", parse(from_os_str))]
    manifest_path: Option<PathBuf>,
    #[structopt(long)]
    no_default_features: bool,
    #[structopt(long)]
    all_features: bool,
    #[structopt(long)]
    features: Vec<String>,
}

const ARGUMENTS_TO_REMOVE: &[&str] = &[
    "chip=",
    "speed=",
    "restore-unwritten",
    "flash-layout=",
    "chip-description-path=",
    "list-chips",
    "list-probes",
    "probe=",
    "elf=",
    "work-dir=",
    "disable-progressbars",
    "protocol=",
    "probe-index=",
    "reset-halt",
    "nrf-recover",
    "log=",
    "connect-under-reset",
    "dry-run",
];

fn main() {
    let next = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        #[cfg(feature = "sentry")]
        if ask_to_log_crash() {
            capture_panic(&METADATA.lock().unwrap(), &info)
        }
        #[cfg(not(feature = "sentry"))]
        log::info!("{:#?}", &METADATA.lock().unwrap());
        next(info);
    }));

    match main_try() {
        Ok(_) => (),
        Err(e) => {
            #[cfg(not(feature = "sentry"))]
            log::info!("{:#?}", &METADATA.lock().unwrap());

            // Ensure stderr is flushed before calling proces::exit,
            // otherwise the process might panic, because it tries
            // to access stderr during shutdown.
            //
            // We ignore the errors, not much we can do anyway.
            e.render();

            #[cfg(feature = "sentry")]
            if ask_to_log_crash() {
                capture_anyhow(&METADATA.lock().unwrap(), e.source_error())
            }

            process::exit(1);
        }
    }
}

fn main_try() -> Result<(), Diagnostic> {
    let mut args = std::env::args();

    // When called by Cargo, the first argument after the binary name will be `flash`. If that's the
    // case, remove one argument (`Opt::from_iter` will remove the binary name by itself).
    if env::args().nth(1) == Some("flash".to_string()) {
        args.next();
    }

    let mut args: Vec<_> = args.collect();

    // Get commandline options.
    let opt = Opt::from_iter(&args);

    logging::init(opt.log);

    let work_dir = PathBuf::from(opt.work_dir.clone().unwrap_or_else(|| ".".to_owned()));

    // If someone wants to list the connected probes, just do that and exit.
    if opt.list_probes {
        list_connected_devices()?;
        return Ok(());
    }

    // Load cargo manifest if available and parse out meta object
    let meta = read_metadata(&work_dir).ok();

    // Make sure we load the config given in the cli parameters.
    if let Some(ref cdp) = opt.chip_description_path {
        probe_rs::config::add_target_from_yaml(&Path::new(cdp))
            .with_context(|| format!("Failed to parse chip description ''"))?;
    }

    let chip = if opt.list_chips {
        print_families()?;
        return Ok(());
    } else {
        // First use command line, then manifest, then default to auto
        match (&opt.chip, meta.map(|m| m.chip).flatten()) {
            (Some(c), _) => c.into(),
            (_, Some(c)) => c.into(),
            _ => TargetSelector::Auto,
        }
    };
    METADATA.lock().unwrap().chip = Some(format!("{:?}", chip));

    args.remove(0); // Remove executable name

    // Remove all arguments that `cargo build` does not understand.
    argument_handling::remove_arguments(ARGUMENTS_TO_REMOVE, &mut args);

    // Change the work dir if the user asked to do so
    //std::env::set_current_dir(&work_dir).context("Failed to change the working directory")?;

    log::debug!("Changed working directory to {}", work_dir.display());

    let path: PathBuf = if let Some(path) = &opt.elf {
        path.into()
    } else {
        // Build the project, and extract the path of the built artifact.
        build_artifact(&work_dir, &args).with_context(|| {
            if let Some(ref work_dir) = opt.work_dir {
                format!("Failed to build Cargo project in directory '{}'", work_dir)
            } else {
                "Failed to build Cargo project".to_string()
            }
        })?
    };

    logging::println(format!(
        "    {} {}",
        "Flashing".green().bold(),
        path.display()
    ));

    let mut data_buffer = Vec::new();

    let (target_selector, flash_loader) = if let Some(chip_name) = &opt.chip {
        let target = probe_rs::config::get_target_by_name(chip_name)
            .context("Failed to retrieve chip description.")?;

        let loader = build_flashloader(&target, &path, &mut data_buffer)
            .map_err(|e| handle_flash_error(e, &target, opt.chip.as_deref()))?;
        (TargetSelector::Specified(target), Some(loader))
    } else {
        (TargetSelector::Auto, None)
    };

    let mut probe = open_probe(&opt)?;

    probe
        .select_protocol(opt.protocol)
        .context("failed to select protocol")?;

    let protocol_speed = if let Some(speed) = opt.speed {
        let actual_speed = probe.set_speed(speed).context("failed to set speed")?;

        if actual_speed < speed {
            log::warn!(
                "Unable to use specified speed of {} kHz, actual speed used is {} kHz",
                speed,
                actual_speed
            );
        }

        actual_speed
    } else {
        probe.speed_khz()
    };

    METADATA.lock().unwrap().speed = Some(format!("{:?}", protocol_speed));

    log::info!("Protocol speed {} kHz", protocol_speed);

    let mut session = if opt.connect_under_reset {
        probe
            .attach_under_reset(target_selector)
            .context("failed attaching to target")?
    } else {
        let potential_session = probe.attach(target_selector);
        match potential_session {
            Ok(session) => session,
            Err(err) => {
                let hint = match err {
                    probe_rs::Error::ChipNotFound(
                        probe_rs::config::RegistryError::ChipAutodetectFailed,
                    ) => {
                        let autodetection_hint = "Specify a chip using the `--chip` option. \n \
                                                       A list of all supported chips can be shown using the `--list-chips` command.";
                        Some(autodetection_hint.to_owned())
                    }
                    _ => {
                        let mut buff = String::new();
                        let _ = writeln!(buff, "The target seems to be unable to be attached to.");
                        let _ = writeln!(buff, "A hard reset during attaching might help. This will reset the entire chip.");
                        let _ = writeln!(
                            buff,
                            "Run with `--connect-under-reset` to enable this feature."
                        );
                        Some(buff.to_owned())
                    }
                };

                let mut diagnostic =
                    Diagnostic::from(anyhow!(err).context("Failed attaching to target"));

                if let Some(hint) = hint {
                    diagnostic.add_hint(hint);
                }

                return Err(diagnostic);
            }
        }
    };

    // Start timer.
    let instant = Instant::now();

    run_flash_operation(&mut session, &path, &opt, flash_loader)
        .map_err(|e| handle_flash_error(e, session.target(), opt.chip.as_deref()))?;

    // Stop timer.
    let elapsed = instant.elapsed();
    logging::println(format!(
        "    {} in {}s",
        "Finished".green().bold(),
        elapsed.as_millis() as f32 / 1000.0,
    ));

    {
        let mut core = session
            .core(0)
            .context("Failed to get handle for first core")?;
        if opt.reset_halt {
            core.reset_and_halt(std::time::Duration::from_millis(500))
                .context("failed to reset and halt")?;
        } else {
            core.reset().context("failed to reset")?;
        }
    }

    Ok(())
}

fn print_families() -> Result<()> {
    logging::println("Available chips:");
    for family in probe_rs::config::families().context("failed to read families")? {
        logging::println(&family.name);
        logging::println("    Variants:");
        for variant in family.variants() {
            logging::println(format!("        {}", variant.name));
        }
    }
    Ok(())
}

/// Lists all connected devices
fn list_connected_devices() -> Result<()> {
    let probes = Probe::list_all();

    if !probes.is_empty() {
        println!("The following devices were found:");
        probes
            .iter()
            .enumerate()
            .for_each(|(num, link)| println!("[{}]: {:?}", num, link));
    } else {
        println!("No devices were found.");
    }

    Ok(())
}

fn run_flash_operation(
    session: &mut Session,
    path: &Path,
    opt: &Opt,
    loader: Option<FlashLoader>,
) -> Result<()> {
    let mut buffer = Vec::new();

    let mut loader = match loader {
        Some(loader) => loader,
        None => build_flashloader(session.target(), path, &mut buffer)?,
    };

    if !opt.disable_progressbars {
        // Create progress bars.
        let multi_progress = MultiProgress::new();
        let style = ProgressStyle::default_bar()
                    .tick_chars("⠁⠁⠉⠙⠚⠒⠂⠂⠒⠲⠴⠤⠄⠄⠤⠠⠠⠤⠦⠖⠒⠐⠐⠒⠓⠋⠉⠈⠈✔")
                    .progress_chars("##-")
                    .template("{msg:.green.bold} {spinner} [{elapsed_precise}] [{wide_bar}] {bytes:>8}/{total_bytes:>8} @ {bytes_per_sec:>10} (eta {eta:3})");

        // Create a new progress bar for the fill progress if filling is enabled.
        let fill_progress = if opt.restore_unwritten {
            let fill_progress = Arc::new(multi_progress.add(ProgressBar::new(0)));
            fill_progress.set_style(style.clone());
            fill_progress.set_message("     Reading flash  ");
            Some(fill_progress)
        } else {
            None
        };

        // Create a new progress bar for the erase progress.
        let erase_progress = Arc::new(multi_progress.add(ProgressBar::new(0)));
        {
            logging::set_progress_bar(erase_progress.clone());
        }
        erase_progress.set_style(style.clone());
        erase_progress.set_message("     Erasing sectors");

        // Create a new progress bar for the program progress.
        let program_progress = multi_progress.add(ProgressBar::new(0));
        program_progress.set_style(style);
        program_progress.set_message(" Programming pages  ");

        // Register callback to update the progress.
        let flash_layout_output_path = opt.flash_layout_output_path.clone();
        let progress = FlashProgress::new(move |event| {
            use ProgressEvent::*;
            match event {
                Initialized { flash_layout } => {
                    let total_page_size: u32 = flash_layout.pages().iter().map(|s| s.size()).sum();

                    let total_sector_size: u32 =
                        flash_layout.sectors().iter().map(|s| s.size()).sum();

                    let total_fill_size: u32 = flash_layout.fills().iter().map(|s| s.size()).sum();

                    if let Some(fp) = fill_progress.as_ref() {
                        fp.set_length(total_fill_size as u64)
                    }
                    erase_progress.set_length(total_sector_size as u64);
                    program_progress.set_length(total_page_size as u64);
                    let visualizer = flash_layout.visualize();
                    flash_layout_output_path
                        .as_ref()
                        .map(|path| visualizer.write_svg(path));
                }
                StartedProgramming => {
                    program_progress.enable_steady_tick(100);
                    program_progress.reset_elapsed();
                }
                StartedErasing => {
                    erase_progress.enable_steady_tick(100);
                    erase_progress.reset_elapsed();
                }
                StartedFilling => {
                    if let Some(fp) = fill_progress.as_ref() {
                        fp.enable_steady_tick(100)
                    };
                    if let Some(fp) = fill_progress.as_ref() {
                        fp.reset_elapsed()
                    };
                }
                PageProgrammed { size, .. } => {
                    program_progress.inc(size as u64);
                }
                SectorErased { size, .. } => {
                    erase_progress.inc(size as u64);
                }
                PageFilled { size, .. } => {
                    if let Some(fp) = fill_progress.as_ref() {
                        fp.inc(size as u64)
                    };
                }
                FailedErasing => {
                    erase_progress.abandon();
                    program_progress.abandon();
                }
                FinishedErasing => {
                    erase_progress.finish();
                }
                FailedProgramming => {
                    program_progress.abandon();
                }
                FinishedProgramming => {
                    program_progress.finish();
                }
                FailedFilling => {
                    if let Some(fp) = fill_progress.as_ref() {
                        fp.abandon()
                    };
                }
                FinishedFilling => {
                    if let Some(fp) = fill_progress.as_ref() {
                        fp.finish()
                    };
                }
            }
        });

        // Make the multi progresses print.
        // indicatif requires this in a separate thread as this join is a blocking op,
        // but is required for printing multiprogress.
        let progress_thread_handle = std::thread::spawn(move || {
            multi_progress.join().unwrap();
        });

        loader
            .commit(session, &progress, false, opt.dry_run)
            .with_context(|| format!("failed to flash {}", path.display()))?;

        // We don't care if we cannot join this thread.
        let _ = progress_thread_handle.join();
    } else {
        loader
            .commit(session, &FlashProgress::new(|_| {}), false, opt.dry_run)
            .with_context(|| format!("failed to flash {}", path.display()))?;
    }

    Ok(())
}

fn open_probe(options: &Opt) -> Result<Probe> {
    if options.dry_run {
        return Ok(Probe::from_specific_probe(Box::new(FakeProbe::new())));
    }

    let list = Probe::list_all();

    // If we got a probe selector as an argument, open the probe matching the selector if possible.
    let probe = match &options.probe_selector {
        Some(selector) => Probe::open(selector.clone()).context("Failed to open Probe")?,
        None => {
            // Only automatically select a probe if there is only
            // a single probe detected.
            if list.len() > 1 {
                return Err(anyhow!("More than a single probe detected. Use the --probe argument to select which probe to use.").into());
            }

            Probe::open(
                list.first()
                    .map(|info| {
                        METADATA.lock().unwrap().probe = Some(format!("{:?}", info.probe_type));
                        info
                    })
                    .ok_or_else(|| anyhow!("No supported probe was found"))?,
            )
            .map_err(|err| anyhow!(err))?
        }
    };

    Ok(probe)
}

fn build_flashloader<'data>(
    target: &Target,
    path: &Path,
    buffer: &'data mut Vec<Vec<u8>>,
) -> Result<FlashLoader<'data>> {
    // Now that we have the target, we can create the flash loader

    let mut loader = FlashLoader::new(target.memory_map.to_vec(), true, target.source().clone());

    // Add data from elf.

    let mut file = File::open(&path)
        .with_context(|| format!("Failed to open ELF file {} for flashing.", path.display()))?;

    download_elf(buffer, &mut file, &mut loader).context("Failed to add ELF data")?;

    Ok(loader)
}
