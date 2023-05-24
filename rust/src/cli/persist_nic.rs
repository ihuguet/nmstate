// SPDX-License-Identifier: Apache-2.0

//! # Handling writing .link files for NICs
//!
//! This module implements logic for generating systemd [`.link`] files
//! and kernel arguments based on active networking state.
//!
//! The logic currently is:
//!
//!  - Do nothing if kernel argument contains `net.ifnames=0` which disabled the
//!    predictable network interface name, hence not fit our use case here.
//!  - Iterate over all active NICs
//!  - Pin every Ethernet interface to its MAC address (prefer permanent MAC
//!    address) using link files and the [`ifname=`] kernel argument.
//!  - After booting to new environment, use `udevadm test-builtin net_id` to
//!    check whether pined interface name is different from systemd UDEV
//!    Generated one. If still the same, remove the `.link` file.
//!
//! [`.link`]: https://www.freedesktop.org/software/systemd/man/systemd.link.html
//! [`ifname=`]: https://www.man7.org/linux/man-pages/man7/dracut.cmdline.7.html
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use nmstate::{InterfaceType, NetworkState};

use crate::error::CliError;

/// Comment added into our generated link files
const PERSIST_GENERATED_BY: &str = "# Generated by nmstate";
/// The file prefix for our generated persisted NIC names.
/// 98 here is important as it should be invoked after others but before
/// 99-default.link
const PERSIST_FILE_PREFIX: &str = "98-nmstate";
/// See https://www.freedesktop.org/software/systemd/man/systemd.link.html
const SYSTEMD_NETWORK_LINK_FOLDER: &str = "etc/systemd/network";
/// File which if present signals that we have already performed NIC name
/// persistence.
const NMSTATE_PERSIST_STAMP: &str = ".nmstate-persist.stamp";
const UDEVADM_CMD_OPT: [&str; 2] = ["test-builtin", "net_id"];
const ID_NET_NAME_ONBOARD: &str = "ID_NET_NAME_ONBOARD";
const ID_NET_NAME_SLOT: &str = "ID_NET_NAME_SLOT";
const ID_NET_NAME_PATH: &str = "ID_NET_NAME_PATH";

/// The action to take
pub(crate) enum PersistAction {
    /// Persist NIC name state
    Save,
    /// Print what we would do in Save mode
    DryRun,
    /// Remove link files not required
    CleanUp,
    /// Print what we would do in clean up mode
    CleanUpDryRun,
}

fn gather_state() -> Result<NetworkState, CliError> {
    let mut state = NetworkState::new();
    state.set_kernel_only(true);
    state.set_running_config_only(true);
    state.retrieve()?;
    Ok(state)
}

pub(crate) fn run_persist_immediately(
    root: &str,
    kargsfile: Option<&str>,
    action: PersistAction,
) -> Result<String, CliError> {
    let dry_run = match action {
        PersistAction::Save => false,
        PersistAction::DryRun => true,
        PersistAction::CleanUp => return clean_up(root, kargsfile, false),
        PersistAction::CleanUpDryRun => return clean_up(root, kargsfile, true),
    };

    if is_predictable_ifname_disabled() {
        log::info!(
            "systemd predictable network interface name is disabled \
            by kernel argument `net.ifnames=0`, will do nothing"
        );
        return Ok("".to_string());
    }

    let stamp_path = Path::new(root)
        .join(SYSTEMD_NETWORK_LINK_FOLDER)
        .join(NMSTATE_PERSIST_STAMP);
    if stamp_path.exists() {
        log::info!("{} exists; nothing to do", stamp_path.display());
        return Ok("".to_string());
    }

    let mut kargs: Vec<String> = Vec::new();
    let state = gather_state()?;
    let mut changed = false;
    for iface in state
        .interfaces
        .iter()
        .filter(|i| i.iface_type() == InterfaceType::Ethernet)
    {
        // Prefer permanent(often stored in firmware) MAC address
        let mac = match iface
            .base_iface()
            .permanent_mac_address
            .as_deref()
            .or_else(|| iface.base_iface().mac_address.as_deref())
        {
            Some(m) => m,
            None => continue,
        };
        let iface_name = iface.name();
        let karg = format_ifname_karg(iface_name, mac);
        log::info!(
            "Will persist the interface {iface_name} with MAC {mac} \
            using link file and kernel argument {karg}"
        );
        if !dry_run {
            changed |=
                persist_iface_name_via_systemd_link(root, mac, iface_name)?;
            log::info!("Kernel argument {karg} appended");
            kargs.push(karg);
        }
    }

    if !changed {
        log::info!("No changes.");
    }

    if !dry_run {
        std::fs::write(stamp_path, b"")?;
        if !kargs.is_empty() {
            if let Some(path) = kargsfile {
                std::fs::write(path, kargs.join(" "))?;
            }
        }
    }

    Ok("".to_string())
}

fn gen_link_file_path(root: &str, iface_name: &str) -> PathBuf {
    let link_dir = Path::new(root).join(SYSTEMD_NETWORK_LINK_FOLDER);

    link_dir.join(format!("{PERSIST_FILE_PREFIX}-{iface_name}.link"))
}

fn extract_iface_names_from_link_file(file_name: &str) -> Option<String> {
    file_name
        .strip_prefix(&format!("{PERSIST_FILE_PREFIX}-"))
        .and_then(|name| name.strip_suffix(".link"))
        .map(ToOwned::to_owned)
}

pub(crate) fn clean_up(
    root: &str,
    kargsfile: Option<&str>,
    dry_run: bool,
) -> Result<String, CliError> {
    let netdir = Path::new(root).join(SYSTEMD_NETWORK_LINK_FOLDER);

    if !netdir.exists() {
        log::info!("{} does not exist, no need to clean up", netdir.display());
    }
    let stamp_path = netdir.join(NMSTATE_PERSIST_STAMP);
    if !stamp_path.exists() {
        log::info!(
            "{} does not exist, no prior persisted state, no need to clean up",
            stamp_path.display()
        );
        return Ok("".to_string());
    }

    let mut pinned_ifaces: HashMap<String, PathBuf> = HashMap::new();

    for e in netdir.read_dir()? {
        let e = e?;
        let file_name = if let Some(n) = e.file_name().to_str() {
            n.to_string()
        } else {
            continue;
        };
        if let Some(iface_name) = extract_iface_names_from_link_file(&file_name)
        {
            log::info!("Found persisted NIC({iface_name}) file: {file_name}");
            pinned_ifaces
                .insert(iface_name.to_string(), netdir.join(file_name));
        }
    }

    if pinned_ifaces.is_empty() {
        log::info!("No persisted NICs found");
        if !dry_run {
            std::fs::remove_file(stamp_path)?;
        }
        return Ok("".to_string());
    }

    let state = gather_state()?;
    let macs: HashMap<&str, &str> = state
        .interfaces
        .iter()
        .filter(|i| i.iface_type() == InterfaceType::Ethernet)
        .filter_map(|i| {
            i.base_iface()
                .permanent_mac_address
                .as_deref()
                .or_else(|| i.base_iface().mac_address.as_deref())
                .map(|m| (i.name(), m))
        })
        .collect();

    let mut kargs: Vec<String> = Vec::new();
    for (iface_name, file_path) in pinned_ifaces {
        if !is_nmstate_generated_systemd_link_file(&file_path) {
            log::info!(
                "File {} is not generated by nmstate, ignoring",
                file_path.display()
            );
            continue;
        }
        let systemd_iface_name =
            match get_systemd_preferred_iface_name(root, &iface_name) {
                Ok(i) => i,
                Err(e) => {
                    log::error!(
                        "Failed to retrieve systemd preferred \
                        iface name for {iface_name}: {e}"
                    );
                    continue;
                }
            };
        if systemd_iface_name == iface_name {
            log::info!("Interface name {iface_name} is unchanged");
            let mac = match macs.get(iface_name.as_str()) {
                Some(mac) => mac,
                None => {
                    log::error!("Interface {iface_name} has no MAC address");
                    continue;
                }
            };
            let karg = format_ifname_karg(&iface_name, mac);
            log::info!(
                "Will remove generated file {} and kernel argument {karg}",
                file_path.display()
            );
            if !dry_run {
                std::fs::remove_file(&file_path)?;
                log::info!(
                    "Removed systemd network link file {}",
                    file_path.display()
                );
                log::info!("Kernel argument {karg} removed");
                kargs.push(karg);
            }
        } else {
            log::info!(
                "systemd generated interface name \
                '{systemd_iface_name}' != pinned name '{iface_name}', \
                will keep config file {}",
                file_path.display()
            );
        }
    }
    if !dry_run {
        std::fs::remove_file(stamp_path)?;
        if !kargs.is_empty() {
            if let Some(path) = kargsfile {
                std::fs::write(path, kargs.join(" "))?;
            }
        }
    }
    Ok("".to_string())
}

fn format_ifname_karg(ifname: &str, mac: &str) -> String {
    format!("ifname={ifname}:{mac}")
}

// With `NamePolicy=keep kernel database onboard slot path` in systemd configure
// in RHEL 8 and 9. Assuming `keep, kernel and database` all return NULL,
// systemd will use interface name in the order of:
//  * `ID_NET_NAME_ONBOARD`
//  * `ID_NET_NAME_SLOT`
//  * `ID_NET_NAME_PATH`
pub(crate) fn get_systemd_preferred_iface_name(
    root: &str,
    iface_name: &str,
) -> Result<String, CliError> {
    let mut cmd = if root == "/" {
        std::process::Command::new("udevadm")
    } else {
        std::process::Command::new("chroot")
    };
    if root != "/" {
        cmd.arg(root).arg("udevadm");
    }
    cmd.args(UDEVADM_CMD_OPT)
        .arg(&format!("/sys/class/net/{iface_name}"));
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(CliError::from(format!(
            "Command {:?} failed with error: {}",
            cmd,
            String::from_utf8(output.stderr).unwrap_or_default()
        )));
    }
    let output: String = String::from_utf8(output.stdout).map_err(|e| {
        CliError::from(format!("Failed to parse udevadm reply to UTF-8: {e}"))
    })?;

    let lines = output.lines().filter_map(|l| l.split_once('='));
    for (k, v) in lines.clone() {
        if k == ID_NET_NAME_ONBOARD {
            return Ok(v.to_string());
        }
    }
    for (k, v) in lines.clone() {
        if k == ID_NET_NAME_SLOT {
            return Ok(v.to_string());
        }
    }
    for (k, v) in lines.clone() {
        if k == ID_NET_NAME_PATH {
            return Ok(v.to_string());
        }
    }

    Err(format!(
        "Failed to retrieve interface name from udevadm command: {}",
        output
    )
    .into())
}

fn persist_iface_name_via_systemd_link(
    root: &str,
    mac: &str,
    iface_name: &str,
) -> Result<bool, CliError> {
    let link_dir = Path::new(root).join(SYSTEMD_NETWORK_LINK_FOLDER);

    let file_path = gen_link_file_path(root, iface_name);
    if file_path.exists() {
        log::info!("Network link file {} already exists", file_path.display());
        return Ok(false);
    }

    if !link_dir.exists() {
        std::fs::create_dir(&link_dir)?;
    }

    let content =
        format!("{PERSIST_GENERATED_BY}\n[Match]\nMACAddress={mac}\n\n[Link]\nName={iface_name}\n");

    std::fs::write(&file_path, content.as_bytes()).map_err(|e| {
        CliError::from(format!(
            "Failed to store captured states to file {}: {e}",
            file_path.display()
        ))
    })?;
    log::info!(
        "systemd network link file created at {}",
        file_path.display()
    );
    Ok(true)
}

fn is_nmstate_generated_systemd_link_file(file_path: &PathBuf) -> bool {
    let mut buff = [0; PERSIST_GENERATED_BY.len()];

    std::fs::File::open(file_path)
        .and_then(|mut fd| fd.read_exact(&mut buff))
        .ok()
        .map(|_| buff == PERSIST_GENERATED_BY.as_bytes())
        .unwrap_or_default()
}

const KERNEL_CMDLINE_FILE: &str = "/proc/cmdline";

fn is_predictable_ifname_disabled() -> bool {
    std::fs::read(KERNEL_CMDLINE_FILE)
        .map(|v| String::from_utf8(v).unwrap_or_default())
        .map(|c| c.contains("net.ifnames=0"))
        .unwrap_or_default()
}
