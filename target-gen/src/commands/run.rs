use std::path::Path;
use std::rc::Rc;
use std::time::Instant;
use std::{cell::RefCell, path::PathBuf};

use anyhow::Result;
use colored::Colorize;
use probe_rs::flashing::erase_all;
use probe_rs::MemoryInterface;
use probe_rs::{
    flashing::{erase_sectors, DownloadOptions, FlashLoader, FlashProgress},
    Permissions, Session,
};
use probe_rs_cli_util::logging::println;

use super::export::{cmd_export, DEFINITION_EXPORT_PATH};

pub fn cmd_run(target_artifact: PathBuf) -> Result<()> {
    // Generate the binary
    println("Generating the YAML file in `target/definition.yaml`");
    cmd_export(target_artifact)?;

    probe_rs::config::add_target_from_yaml(Path::new(DEFINITION_EXPORT_PATH))?;
    let mut session =
        probe_rs::Session::auto_attach("algorithm-test", Permissions::new().allow_erase_all())?;

    let data_size = probe_rs::config::get_target_by_name("algorithm-test")?.flash_algorithms[0]
        .flash_properties
        .page_size;

    // Register callback to update the progress.
    let t = Rc::new(RefCell::new(Instant::now()));
    let progress = FlashProgress::new(move |event| {
        use probe_rs::flashing::ProgressEvent::*;
        match event {
            StartedProgramming => {
                let mut t = t.borrow_mut();
                *t = Instant::now();
            }
            StartedErasing => {
                let mut t = t.borrow_mut();
                *t = Instant::now();
            }
            FailedErasing => {
                println!("Failed erasing in {:?}", t.borrow().elapsed());
            }
            FinishedErasing => {
                println!("Finished erasing in {:?}", t.borrow().elapsed());
            }
            FailedProgramming => {
                println!("Failed programming in {:?}", t.borrow().elapsed());
            }
            FinishedProgramming => {
                println!("Finished programming in {:?}", t.borrow().elapsed());
            }
            Rtt { channel, message } => {
                let rtt = "RTT".yellow();
                let channel = channel.blue();
                if message.ends_with('\n') {
                    print!("{rtt}[{channel}]: {message}");
                } else {
                    println!("{rtt}[{channel}]: {message}");
                }
            }
            _ => (),
        }
    });

    let test = "Test".green();
    let flash_properties = session.target().flash_algorithms[0]
        .flash_properties
        .clone();
    let erased_state = flash_properties.erased_byte_value;

    println!("{test}: Erasing sectorwise and writing two pages ...");

    run_flash_erase(&mut session, &progress, false)?;
    let mut readback = vec![0; flash_properties.sectors[0].size as usize];
    session.core(0)?.read_8(0x0, &mut readback)?;
    assert!(
        !readback.iter().any(|v| *v != erased_state),
        "Not all sectors were erased"
    );

    let mut loader = session.target().flash_loader();
    let data = (0..data_size)
        .into_iter()
        .map(|n| (n % 256) as u8)
        .collect::<Vec<_>>();
    loader.add_data(0x1, &data)?;
    run_flash_download(&mut session, loader, &progress, true)?;
    let mut readback = vec![0; data_size as usize];
    session.core(0)?.read_8(0x1, &mut readback)?;
    assert_eq!(readback, data);

    println!("{test}: Erasing the entire chip and writing two pages ...");
    run_flash_erase(&mut session, &progress, true)?;
    let mut readback = vec![0; flash_properties.sectors[0].size as usize];
    session.core(0)?.read_8(0x0, &mut readback)?;
    assert!(
        !readback.iter().any(|v| *v != erased_state),
        "Not all sectors were erased"
    );

    let mut loader = session.target().flash_loader();
    let data = (0..data_size)
        .into_iter()
        .map(|n| (n % 256) as u8)
        .collect::<Vec<_>>();
    loader.add_data(0x1, &data)?;
    run_flash_download(&mut session, loader, &progress, true)?;
    let mut readback = vec![0; data_size as usize];
    session.core(0)?.read_8(0x1, &mut readback)?;
    assert_eq!(readback, data);

    println!("{test}: Erasing sectorwise and writing two pages double buffered ...");
    run_flash_erase(&mut session, &progress, false)?;
    let mut readback = vec![0; flash_properties.sectors[0].size as usize];
    session.core(0)?.read_8(0x0, &mut readback)?;
    assert!(
        !readback.iter().any(|v| *v != erased_state),
        "Not all sectors were erased"
    );

    let mut loader = session.target().flash_loader();
    let data = (0..data_size)
        .into_iter()
        .map(|n| (n % 256) as u8)
        .collect::<Vec<_>>();
    loader.add_data(0x1, &data)?;
    run_flash_download(&mut session, loader, &progress, false)?;
    let mut readback = vec![0; data_size as usize];
    session.core(0)?.read_8(0x1, &mut readback)?;
    assert_eq!(readback, data);

    Ok(())
}

/// Performs the flash download with the given loader. Ensure that the loader has the data to load already stored.
/// This function also manages the update and display of progress bars.
pub fn run_flash_download(
    session: &mut Session,
    loader: FlashLoader,
    progress: &FlashProgress,
    disable_double_buffering: bool,
) -> Result<()> {
    let mut download_option = DownloadOptions::default();
    download_option.keep_unwritten_bytes = false;
    download_option.disable_double_buffering = disable_double_buffering;

    download_option.progress = Some(progress);
    download_option.skip_erase = true;

    loader.commit(session, download_option)?;

    Ok(())
}

/// Erases the given flash sectors.
pub fn run_flash_erase(
    session: &mut Session,
    progress: &FlashProgress,
    do_chip_erase: bool,
) -> Result<()> {
    if do_chip_erase {
        erase_all(session, Some(progress))?;
    } else {
        erase_sectors(session, Some(progress), 0, 2)?;
    }

    Ok(())
}
