/// Dispatches deliveries for plugins without an external source loop.
async fn serve<G: exports::pluribus::plugin::lifecycle::Guest>(
    mut context: exports::pluribus::plugin::lifecycle::Context,
) -> Result<(), pluribus::plugin::types::Error> {
    use pluribus::plugin::runtime;
    loop {
        match runtime::next().await? {
            runtime::Wake::Events(events) => match G::handle(context.clone(), events).await {
                Ok(outcome) => {
                    runtime::commit(&outcome.events, &outcome.mutations, outcome.checkpoint)?;
                    if let Some(checkpoint) = outcome.checkpoint {
                        context.state_checkpoint = checkpoint;
                    }
                }
                Err(error) => runtime::reject(&error)?,
            },
            runtime::Wake::Stop(_) => return Ok(()),
        }
    }
}
