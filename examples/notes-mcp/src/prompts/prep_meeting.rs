//! Meeting preparation prompt

use std::collections::HashMap;

use tower_mcp::{GetPromptResult, Prompt, PromptBuilder, PromptMessage, PromptRole};

pub fn build() -> Prompt {
    PromptBuilder::new("prep_meeting")
        .description("Prepare a briefing for an upcoming customer meeting")
        .required_arg("customer_name", "Name of the customer to prepare for")
        .optional_arg("focus", "Specific topic or area to focus on")
        .handler(|args: HashMap<String, String>| async move {
            let customer_name = args
                .get("customer_name")
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            let focus = args.get("focus");

            let mut prompt = format!(
                "Please prepare a briefing for an upcoming meeting with {customer_name}.\n\n\
                 Follow these steps using the available tools:\n\n\
                 1. **Find the customer**: Use `search_customers` to look up \"{customer_name}\"\n\
                 2. **Get full profile**: Use `get_customer` with their ID to see their profile and notes\n\
                 3. **Search for relevant notes**: Use `search_notes` to find any additional context\n\
                 4. **Summarize**: Provide a meeting briefing that includes:\n\
                    - Customer overview (company, role, tier)\n\
                    - Relationship timeline and key interactions\n\
                    - Open items and unresolved issues\n\
                    - Suggested talking points\n\
                    - Risks or opportunities to be aware of"
            );

            if let Some(topic) = focus {
                prompt.push_str(&format!(
                    "\n\n**Focus area:** Please pay special attention to \"{topic}\" \
                     when searching notes and preparing the briefing."
                ));
            }

            Ok(GetPromptResult {
                description: Some(format!("Meeting prep for {customer_name}")),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: tower_mcp::protocol::Content::Text {
                        text: prompt,
                        annotations: None,
                        meta: None,
                    },
                    meta: None,
                }],
                meta: None,
            })
        })
        .build()
}
