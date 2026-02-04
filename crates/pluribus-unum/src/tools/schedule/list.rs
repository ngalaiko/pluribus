use pluribus_frequency::state::Configuration;

pub struct Tool {
    config: Configuration,
}

pub const fn new(config: Configuration) -> Tool {
    Tool { config }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {}

#[derive(serde::Serialize)]
pub struct ScheduleEntry {
    id: String,
    prompt: String,
    trigger: String,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Vec<ScheduleEntry>;

    fn def(&self) -> pluribus_frequency::protocol::ToolDef {
        pluribus_frequency::protocol::ToolDef::new(
            pluribus_frequency::protocol::ToolName::new("schedule_list"),
            "List all schedules.",
        )
    }

    fn execute(
        &self,
        _input: Input,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<ScheduleEntry>, String>> + Send + '_>,
    > {
        Box::pin(async {
            let entries = super::entries(&self.config);
            let mut result: Vec<ScheduleEntry> = entries
                .into_iter()
                .map(|(id, schedule)| {
                    let trigger = match &schedule.trigger {
                        super::Trigger::Once { at } => format!("once at {at}"),
                        super::Trigger::Cron { cron } => format!("cron: {cron}"),
                    };
                    ScheduleEntry {
                        id,
                        prompt: schedule.prompt,
                        trigger,
                    }
                })
                .collect();
            result.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(result)
        })
    }
}
