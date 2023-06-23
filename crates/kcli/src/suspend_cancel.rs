use clap::Parser;
use kumo_api_types::SuspendV1CancelRequest;
use reqwest::Url;
use uuid::Uuid;

#[derive(Debug, Parser)]
/// Cancels an admin suspend entry.
///
/// Cancelling the entry prevents it from matching new messages.
/// It cannot retroactively un-suspend messages that it already
/// matched and suspendd.
pub struct SuspendCancelCommand {
    /// The id field of the suspend entry that you wish to cancel
    #[arg(long, value_parser=Uuid::parse_str)]
    pub id: Uuid,
}

impl SuspendCancelCommand {
    pub async fn run(&self, endpoint: &Url) -> anyhow::Result<()> {
        let response = crate::request_with_text_response(
            reqwest::Method::DELETE,
            endpoint.join("/api/admin/suspend/v1")?,
            &SuspendV1CancelRequest { id: self.id },
        )
        .await?;

        if !response.is_empty() {
            println!("{response}");
        } else {
            println!("OK");
        }

        Ok(())
    }
}
