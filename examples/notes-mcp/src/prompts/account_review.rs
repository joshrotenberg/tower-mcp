//! Account review prompt — tier-level portfolio analysis

use std::collections::HashMap;

use tower_mcp::{GetPromptResult, Prompt, PromptBuilder, PromptMessage, PromptRole};

pub fn build() -> Prompt {
    PromptBuilder::new("account_review")
        .description("Review all accounts in a specific tier with themes and action items")
        .required_arg("tier", "Account tier to review: enterprise, startup, or smb")
        .optional_arg("focus", "Specific area to focus on (e.g. renewals, onboarding, risk)")
        .handler(|args: HashMap<String, String>| async move {
            let tier = args
                .get("tier")
                .map(|s| s.as_str())
                .unwrap_or("enterprise");
            let focus = args.get("focus");

            let mut prompt = format!(
                "Please perform a portfolio-level review of all **{tier}** accounts.\n\n\
                 Follow these steps using the available tools:\n\n\
                 1. **List all customers**: Use `list_customers` to see every customer and their note counts\n\
                 2. **Filter by tier**: Identify the {tier}-tier customers from the list\n\
                 3. **Deep dive each account**: For each {tier} customer, use `get_customer` to pull their full profile and notes\n\
                 4. **Search for themes**: Use `search_notes` to look for common themes across {tier} accounts \
                    (e.g. renewals, escalations, feature requests)\n\
                 5. **Produce a portfolio summary** that includes:\n\
                    - Overview of the {tier} portfolio (number of accounts, key contacts)\n\
                    - Account-by-account status summary\n\
                    - Common themes and patterns across accounts\n\
                    - Risk flags (unhappy customers, stalled deals, overdue follow-ups)\n\
                    - Opportunities (upsell candidates, expansion signals, advocacy potential)\n\
                    - Prioritized action items with specific next steps"
            );

            if let Some(topic) = focus {
                prompt.push_str(&format!(
                    "\n\n**Focus area:** Pay special attention to \"{topic}\" \
                     when reviewing notes and summarizing themes."
                ));
            }

            Ok(GetPromptResult {
                description: Some(format!("Account review for {tier} tier")),
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
