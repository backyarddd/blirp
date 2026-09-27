//! `blirp pair`, `blirp hub`, `blirp devices` (§10). They drive the running
//! daemon's API, which owns the iroh endpoint.

use super::Client;
use blirp_core::model::{Device, MachineRole, SyncInvite, SyncStatus, UpdateNeeded};
use clap::Subcommand;
use reqwest::Method;
use serde_json::json;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum HubAction {
    /// Become the hub and print an invite for the first machine.
    Enable,
    /// Stop being a hub (paired machines stay known for re-enabling).
    Disable,
    /// Create another invite + code (valid 10 minutes, single use).
    Invite,
    /// Show the sync role and connection state.
    Status,
    /// Set up this machine as an always-on hub (a VPS or home server):
    /// autostart that survives logout and reboots, LAN discovery off, hub
    /// role, and an invite for your PC. Safe to run again.
    Setup {
        /// Keep LAN discovery (mDNS) on, for a hub on your home network.
        #[arg(long)]
        lan: bool,
    },
}

#[derive(Subcommand)]
pub enum DevicesAction {
    /// List paired machines and browser devices.
    List,
    /// Revoke a device or machine by id; its connections close immediately.
    Revoke { id: String },
}

/// Why sync is refused when the hub and a machine run releases that
/// replicate different fields (§10), and which one to update.
pub(super) fn release_problems(s: &SyncStatus) -> Vec<String> {
    let mut out = Vec::new();
    match s.update_needed {
        Some(UpdateNeeded::ThisMachine) if s.role == MachineRole::Hub => out.push(
            "a paired machine runs a newer blirp release, so sync with it is refused: \
             update blirp on this machine (the hub) to the same release"
                .to_string(),
        ),
        Some(UpdateNeeded::ThisMachine) => out.push(
            "the hub runs a newer blirp release, so it refuses to sync: update blirp on this \
             machine to the same release"
                .to_string(),
        ),
        Some(UpdateNeeded::Hub) => out.push(
            "the hub runs an older blirp release, so it refuses to sync: update blirp on the \
             hub to the same release"
                .to_string(),
        ),
        None => {}
    }
    if !s.outdated_machines.is_empty() {
        out.push(format!(
            "paired machine(s) {} run an older blirp release, so their sync is refused: update \
             blirp on them to the same release",
            s.outdated_machines.join(", ")
        ));
    }
    out
}

fn print_status(s: &SyncStatus) {
    println!("role       {}", s.role);
    println!("machine    {}", s.machine_id);
    if let Some(h) = &s.hub {
        println!("hub        {h}");
    }
    if s.role != MachineRole::Standalone {
        println!("connected  {}", if s.connected { "yes" } else { "no" });
    }
    if s.role == MachineRole::Node {
        println!("pending    {} change(s) to send", s.pending_outbox);
    }
    if let (Some(url), Some(fp)) = (&s.portal_url, &s.portal_cert_fingerprint) {
        println!("portal     {url}");
        println!("cert       {fp}");
    }
    for p in release_problems(s) {
        println!("refused    {p}");
    }
}

fn print_invite(i: &SyncInvite) {
    println!("On the other machine run:");
    println!();
    println!("  blirp pair {} {}", i.invite, i.code);
    println!();
    println!("Code {} is valid for 10 minutes and works once.", i.code);
}

pub async fn hub(client: &Client, action: HubAction) -> anyhow::Result<ExitCode> {
    match action {
        HubAction::Enable => {
            let s: SyncStatus = client
                .send(Method::POST, "/api/sync/hub/enable", None)
                .await?
                .json()
                .await?;
            print_status(&s);
            println!();
            let i: SyncInvite = client
                .send(Method::POST, "/api/sync/invite", None)
                .await?
                .json()
                .await?;
            print_invite(&i);
        }
        HubAction::Disable => {
            let s: SyncStatus = client
                .send(Method::POST, "/api/sync/hub/disable", None)
                .await?
                .json()
                .await?;
            print_status(&s);
        }
        HubAction::Invite => {
            let i: SyncInvite = client
                .send(Method::POST, "/api/sync/invite", None)
                .await?
                .json()
                .await?;
            print_invite(&i);
        }
        HubAction::Status => print_status(&client.get("/api/sync/status").await?),
        // Dispatched before a client exists (it starts the daemon).
        HubAction::Setup { .. } => return Ok(ExitCode::from(2)),
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn pair(
    client: &Client,
    first: String,
    code: Option<String>,
) -> anyhow::Result<ExitCode> {
    // `blirp pair <code>` alone searches the LAN; a join link carries both.
    let (invite, code) = match code {
        Some(c) => (first, c),
        None if first.starts_with("blirp://") => (first, String::new()),
        None => (String::new(), first),
    };
    println!("Pairing with the hub...");
    let s: SyncStatus = client
        .send(
            Method::POST,
            "/api/sync/join",
            Some(json!({"invite": invite, "code": code})),
        )
        .await?
        .json()
        .await?;
    println!("Paired. This machine now syncs with the hub.");
    print_status(&s);
    Ok(ExitCode::SUCCESS)
}

pub async fn devices(client: &Client, action: DevicesAction) -> anyhow::Result<ExitCode> {
    match action {
        DevicesAction::List => {
            let list: Vec<Device> = client.get("/api/devices").await?;
            if list.is_empty() {
                // The role only picks the hint: without it, the neutral one.
                let role = match client.get::<SyncStatus>("/api/sync/status").await {
                    Ok(s) => Some(s.role),
                    Err(e) => {
                        eprintln!("warning: reading the sync role failed: {e:#}");
                        None
                    }
                };
                println!(
                    "{}",
                    match role {
                        None => "no devices",
                        Some(MachineRole::Node) => {
                            "no devices here: the hub keeps them (run `blirp devices list` on the hub)"
                        }
                        Some(MachineRole::Hub) => {
                            "no devices yet (pair machines with `blirp hub invite`)"
                        }
                        Some(MachineRole::Standalone) => {
                            "no devices (make a machine the hub with `blirp hub enable`, then pair others with it)"
                        }
                    }
                );
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "{:<36}  {:<8}  {:<8}  {:<7}  NAME",
                "ID", "KIND", "STATE", "CONTROL"
            );
            for d in list {
                println!(
                    "{:<36}  {:<8}  {:<8}  {:<7}  {}",
                    d.id,
                    d.kind,
                    if d.revoked { "revoked" } else { "active" },
                    if d.can_control_terminals { "yes" } else { "no" },
                    d.name
                );
            }
        }
        DevicesAction::Revoke { id } => {
            client
                .send(Method::DELETE, &format!("/api/devices/{id}"), None)
                .await?;
            println!("revoked {id}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(role: MachineRole, update_needed: Option<UpdateNeeded>) -> SyncStatus {
        SyncStatus {
            role,
            machine_id: "m".into(),
            hub: Some("h".into()),
            connected: false,
            last_sync_at: None,
            pending_outbox: 0,
            portal_url: None,
            portal_cert_fingerprint: None,
            relay_url: None,
            update_needed,
            outdated_machines: Vec::new(),
        }
    }

    #[test]
    fn release_problems_name_the_machine_to_update() {
        assert!(release_problems(&status(MachineRole::Node, None)).is_empty());
        let text = |s: SyncStatus| release_problems(&s).join("\n");
        assert!(
            text(status(MachineRole::Node, Some(UpdateNeeded::Hub)))
                .contains("update blirp on the hub")
        );
        assert!(
            text(status(MachineRole::Node, Some(UpdateNeeded::ThisMachine)))
                .contains("update blirp on this machine")
        );
        assert!(
            text(status(MachineRole::Hub, Some(UpdateNeeded::ThisMachine)))
                .contains("update blirp on this machine (the hub)")
        );
        let hub = SyncStatus {
            outdated_machines: vec!["pc".into()],
            ..status(MachineRole::Hub, None)
        };
        assert!(text(hub).contains("paired machine(s) pc run an older blirp release"));
    }
}
