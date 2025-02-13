use anyhow::anyhow;
use clap::Args;

use ockam::Context;
use ockam_api::cli_state;
use ockam_api::cli_state::identities::IdentityConfig;
use ockam_api::cli_state::traits::{StateDirTrait, StateItemTrait};

use ockam_identity::{IdentityChangeConstants, KeyAttributes};
use ockam_vault::SecretAttributes;

use crate::util::node_rpc;
use crate::CommandGlobalOpts;

/// Attach a key to a vault
#[derive(Clone, Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct AttachKeyCommand {
    /// Name of the vault to attach the key to
    vault: String,

    /// AWS KMS key to attach
    #[arg(short, long)]
    key_id: String,
}

impl AttachKeyCommand {
    pub fn run(self, opts: CommandGlobalOpts) {
        node_rpc(rpc, (opts, self));
    }
}

async fn rpc(
    mut _ctx: Context,
    (opts, cmd): (CommandGlobalOpts, AttachKeyCommand),
) -> crate::Result<()> {
    run_impl(opts, cmd).await
}

async fn run_impl(opts: CommandGlobalOpts, cmd: AttachKeyCommand) -> crate::Result<()> {
    let v_state = opts.state.vaults.get(&cmd.vault)?;
    if !v_state.config().is_aws() {
        return Err(anyhow!("Vault {} is not an AWS KMS vault", cmd.vault).into());
    }
    let vault = v_state.get().await?;
    let idt = {
        let attrs = SecretAttributes::NistP256;
        let key_attrs = KeyAttributes::new(IdentityChangeConstants::ROOT_LABEL.to_string(), attrs);
        opts.state
            .get_identities(vault)
            .await?
            .identities_creation()
            .create_identity_with_existing_key(&cmd.key_id, key_attrs)
            .await?
    };
    let idt_name = cli_state::random_name();
    let idt_config = IdentityConfig::new(&idt).await;
    opts.state.identities.create(&idt_name, idt_config)?;
    println!("Identity attached to vault: {idt_name}");
    Ok(())
}
