use futures_lite::StreamExt;
use pluribus_frequency::state::State;
use pluribus_llm::deepseek::{self, DeepSeek, DeepSeekModel};

/// Resolve the `DeepSeek` LLM: try stored key, wait for peer sync, or
/// prompt the user.
pub async fn resolve(state: &State) -> DeepSeek {
    let config = state.configuration();

    // Try stored key first.
    if let Some(llm) = deepseek::from_config(&config).await {
        return llm;
    }

    // Wait briefly for a peer to sync the key.
    {
        tracing::debug!("waiting for peers to sync DeepSeek API key");
        let mut receiver = state.new_broadcast_receiver();
        let timeout = async_io::Timer::after(std::time::Duration::from_secs(5));
        let got_key = futures_lite::future::or(
            async {
                futures_lite::pin!(timeout);
                (&mut timeout).await;
                false
            },
            async {
                loop {
                    if receiver.next().await.is_none() {
                        return false;
                    }
                    if deepseek::from_config(&config).await.is_some() {
                        return true;
                    }
                }
            },
        )
        .await;

        if got_key {
            if let Some(llm) = deepseek::from_config(&config).await {
                return llm;
            }
            tracing::warn!("DeepSeek API key from peer is invalid");
        } else {
            tracing::debug!("no peer sync received, will prompt for key");
        }
    }

    // Prompt the user.
    loop {
        eprint!("Enter DeepSeek API key: ");
        let key = blocking::unblock(|| {
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf).ok();
            buf.trim().to_owned()
        })
        .await;
        if key.is_empty() {
            continue;
        }
        tracing::debug!("validating DeepSeek API key");
        match DeepSeek::new(&key, DeepSeekModel::Chat).await {
            Ok(llm) => {
                tracing::debug!("DeepSeek API key valid");
                let _ = deepseek::set_api_key(&config, &key);
                return llm;
            }
            Err(e) => tracing::warn!(%e, "DeepSeek API key invalid"),
        }
    }
}
