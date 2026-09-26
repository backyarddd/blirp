//! `blirp pair`, `blirp hub`, `blirp devices` (§10). They drive the running
//! daemon's API, which owns the iroh endpoint.

use super::Client;
use blirp_core::model::{Device, MachineRole, SyncInvite, SyncStatus};
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
}

#[derive(Subcommand)]
pub enum DevicesAction {
    /// List paired machines and browser devices.
    List,
    /// Revoke a device or machine by id; its connections close immediately.
    Revoke { id: String },
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
                println!("no devices (pair machines with `blirp hub enable` on the hub)");
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
