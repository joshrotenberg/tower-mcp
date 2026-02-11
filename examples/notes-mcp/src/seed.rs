//! Index creation and sample data seeding

use redis::aio::MultiplexedConnection;
use serde_json::json;

/// Create RediSearch indexes and load sample data.
///
/// Drops existing indexes first so this is idempotent.
pub async fn seed(mut conn: MultiplexedConnection) -> Result<(), tower_mcp::BoxError> {
    // Drop existing indexes (ignore errors if they don't exist)
    let _ = redis::cmd("FT.DROPINDEX")
        .arg("idx:customers")
        .arg("DD")
        .query_async::<()>(&mut conn)
        .await;
    let _ = redis::cmd("FT.DROPINDEX")
        .arg("idx:notes")
        .arg("DD")
        .query_async::<()>(&mut conn)
        .await;

    // Create customer index
    redis::cmd("FT.CREATE")
        .arg("idx:customers")
        .arg("ON")
        .arg("JSON")
        .arg("PREFIX")
        .arg("1")
        .arg("customer:")
        .arg("SCHEMA")
        .arg("$.name")
        .arg("AS")
        .arg("name")
        .arg("TEXT")
        .arg("WEIGHT")
        .arg("2.0")
        .arg("$.company")
        .arg("AS")
        .arg("company")
        .arg("TEXT")
        .arg("WEIGHT")
        .arg("2.0")
        .arg("$.email")
        .arg("AS")
        .arg("email")
        .arg("TAG")
        .arg("$.role")
        .arg("AS")
        .arg("role")
        .arg("TEXT")
        .arg("$.tier")
        .arg("AS")
        .arg("tier")
        .arg("TAG")
        .query_async::<()>(&mut conn)
        .await?;

    // Create notes index
    redis::cmd("FT.CREATE")
        .arg("idx:notes")
        .arg("ON")
        .arg("JSON")
        .arg("PREFIX")
        .arg("1")
        .arg("note:")
        .arg("SCHEMA")
        .arg("$.content")
        .arg("AS")
        .arg("content")
        .arg("TEXT")
        .arg("$.customerId")
        .arg("AS")
        .arg("customerId")
        .arg("TAG")
        .arg("$.noteType")
        .arg("AS")
        .arg("noteType")
        .arg("TAG")
        .arg("$.tags[*]")
        .arg("AS")
        .arg("tags")
        .arg("TAG")
        .arg("$.createdAt")
        .arg("AS")
        .arg("createdAt")
        .arg("TAG")
        .arg("SORTABLE")
        .query_async::<()>(&mut conn)
        .await?;

    // --- Customers ---
    let customers = vec![
        json!({
            "id": "c1",
            "name": "Sarah Chen",
            "company": "Meridian Systems",
            "email": "sarah.chen@meridiansys.com",
            "role": "VP of Engineering",
            "tier": "enterprise"
        }),
        json!({
            "id": "c2",
            "name": "Marcus Johnson",
            "company": "LaunchPad AI",
            "email": "marcus@launchpadai.com",
            "role": "CTO",
            "tier": "startup"
        }),
        json!({
            "id": "c3",
            "name": "Elena Rodriguez",
            "company": "CraftBrew Supply Co",
            "email": "elena@craftbrewsupply.com",
            "role": "Operations Director",
            "tier": "smb"
        }),
        json!({
            "id": "c4",
            "name": "David Park",
            "company": "Titan Financial Group",
            "email": "dpark@titanfinancial.com",
            "role": "CISO",
            "tier": "enterprise"
        }),
        json!({
            "id": "c5",
            "name": "Priya Sharma",
            "company": "NexGen Robotics",
            "email": "priya@nexgenrobotics.io",
            "role": "Lead Architect",
            "tier": "startup"
        }),
    ];

    for customer in &customers {
        let id = customer["id"].as_str().unwrap();
        redis::cmd("JSON.SET")
            .arg(format!("customer:{id}"))
            .arg("$")
            .arg(customer.to_string())
            .query_async::<()>(&mut conn)
            .await?;
    }

    // --- Notes ---
    // Sarah Chen / Meridian Systems — enterprise renewal arc
    let notes = vec![
        json!({
            "id": "n01",
            "customerId": "c1",
            "content": "Quarterly performance review with Sarah. Meridian's API throughput has improved 40% since migrating to our enterprise tier. She raised concerns about P99 latency on their EU endpoints — promised to escalate to the infrastructure team. Overall satisfaction is high but renewal depends on resolving latency.",
            "noteType": "meeting",
            "tags": ["performance", "latency", "renewal"],
            "createdAt": "2025-09-15T10:30:00Z"
        }),
        json!({
            "id": "n02",
            "customerId": "c1",
            "content": "Infrastructure team deployed edge caching for Meridian's EU traffic. P99 latency dropped from 280ms to 95ms. Sent Sarah the updated metrics dashboard. She was pleased and mentioned this removes a major blocker for their board discussion about expanding our contract.",
            "noteType": "email",
            "tags": ["latency", "infrastructure", "resolved"],
            "createdAt": "2025-10-02T14:00:00Z"
        }),
        json!({
            "id": "n03",
            "customerId": "c1",
            "content": "Sarah shared Meridian's plans to expand into APAC markets. They need data residency guarantees for Singapore and Tokyo regions. Asked about our compliance certifications for those regions. Need to loop in legal team for the data processing agreement.",
            "noteType": "call",
            "tags": ["expansion", "compliance", "apac"],
            "createdAt": "2025-10-20T09:00:00Z"
        }),
        json!({
            "id": "n04",
            "customerId": "c1",
            "content": "Renewal negotiation kickoff. Meridian wants a 3-year enterprise agreement covering NA, EU, and APAC. Sarah is pushing for volume-based pricing tied to API calls rather than flat tier. Proposed 15% discount for 3-year commitment. She'll take it to their procurement team.",
            "noteType": "meeting",
            "tags": ["renewal", "negotiation", "pricing"],
            "createdAt": "2025-11-05T11:00:00Z"
        }),
        // Marcus Johnson / LaunchPad AI — startup evaluation arc
        json!({
            "id": "n05",
            "customerId": "c2",
            "content": "Product demo for Marcus and the LaunchPad AI engineering team. They're building an AI-powered code review tool and need real-time streaming APIs. Very impressed with our WebSocket support. Main concern is cost at scale — they're pre-revenue and burning through their seed round.",
            "noteType": "meeting",
            "tags": ["demo", "websocket", "pricing"],
            "createdAt": "2025-08-20T15:00:00Z"
        }),
        json!({
            "id": "n06",
            "customerId": "c2",
            "content": "Deep-dive technical session with LaunchPad's senior engineers. Walked through our SDK integration, webhook reliability, and retry semantics. They found a bug in our Python SDK's async context manager — filed internally as ENGR-4521. Marcus appreciated the transparency.",
            "noteType": "call",
            "tags": ["technical", "sdk", "bug"],
            "createdAt": "2025-09-05T13:30:00Z"
        }),
        json!({
            "id": "n07",
            "customerId": "c2",
            "content": "Marcus requested a 60-day trial extension. Their Series A timeline slipped and they need more time to validate unit economics with our platform. Approved the extension through end of November. He committed to a decision by December 1st.",
            "noteType": "email",
            "tags": ["trial", "extension", "timeline"],
            "createdAt": "2025-09-22T10:00:00Z"
        }),
        json!({
            "id": "n08",
            "customerId": "c2",
            "content": "Great news — LaunchPad AI closed their Series A at $12M. Marcus confirmed they want to move to a paid plan. Starting with our startup tier but expects to grow quickly. He wants to discuss startup program pricing and whether we offer equity-for-credits arrangements.",
            "noteType": "call",
            "tags": ["series-a", "conversion", "startup-program"],
            "createdAt": "2025-10-30T16:00:00Z"
        }),
        // Elena Rodriguez / CraftBrew Supply Co — SMB success arc
        json!({
            "id": "n09",
            "customerId": "c3",
            "content": "Onboarding call with Elena. CraftBrew is migrating from a spreadsheet-based inventory system. She's not very technical but extremely organized. Set up their initial workspace and walked through the dashboard. She picked up the API concepts quickly — scheduled follow-up in two weeks.",
            "noteType": "meeting",
            "tags": ["onboarding", "migration", "training"],
            "createdAt": "2025-07-10T09:00:00Z"
        }),
        json!({
            "id": "n10",
            "customerId": "c3",
            "content": "Elena independently integrated our inventory webhook with their Shopify store using our Zapier connector. No support tickets filed. She documented her setup process and shared it with other SMB customers in our community forum. This is exactly the kind of self-service success story we want.",
            "noteType": "general",
            "tags": ["integration", "self-service", "community"],
            "createdAt": "2025-08-15T11:00:00Z"
        }),
        json!({
            "id": "n11",
            "customerId": "c3",
            "content": "CraftBrew is expanding to a second warehouse. Elena wants to know about multi-location inventory sync and whether our SMB plan supports it. Confirmed it does — up to 5 locations. She's also interested in our reporting add-on for their accountant.",
            "noteType": "call",
            "tags": ["expansion", "multi-location", "upsell"],
            "createdAt": "2025-09-28T14:30:00Z"
        }),
        json!({
            "id": "n12",
            "customerId": "c3",
            "content": "Marketing team flagged CraftBrew as a case study candidate. Elena is enthusiastic about participating — she says our platform saved them 15 hours per week on inventory management. Scheduled a case study interview for next month. Great NPS score of 9.",
            "noteType": "general",
            "tags": ["case-study", "nps", "marketing"],
            "createdAt": "2025-10-15T10:00:00Z"
        }),
        // David Park / Titan Financial Group — enterprise compliance arc
        json!({
            "id": "n13",
            "customerId": "c4",
            "content": "Initial security review meeting with David and Titan's security team. They require SOC 2 Type II, FIPS 140-2 encryption at rest, and detailed audit logging. Shared our compliance documentation package. David wants penetration test results from the last 12 months.",
            "noteType": "meeting",
            "tags": ["security", "compliance", "soc2"],
            "createdAt": "2025-08-05T10:00:00Z"
        }),
        json!({
            "id": "n14",
            "customerId": "c4",
            "content": "Completed Titan's 200-question security questionnaire. David's team had follow-ups on our key rotation policy and incident response SLA. Provided details on our 4-hour response commitment for P1 incidents. He's satisfied but needs sign-off from their external auditor.",
            "noteType": "email",
            "tags": ["security-questionnaire", "incident-response", "audit"],
            "createdAt": "2025-09-12T09:00:00Z"
        }),
        json!({
            "id": "n15",
            "customerId": "c4",
            "content": "David requested details on our disaster recovery and business continuity plans. Specifically interested in RPO/RTO guarantees and cross-region failover capabilities. Sent our DR runbook and offered to do a live failover demo. He wants to include this in their board risk assessment.",
            "noteType": "call",
            "tags": ["disaster-recovery", "business-continuity", "risk"],
            "createdAt": "2025-10-08T15:00:00Z"
        }),
        // Priya Sharma / NexGen Robotics — startup technical arc
        json!({
            "id": "n16",
            "customerId": "c5",
            "content": "SDK consultation with Priya. NexGen is building a fleet management system for autonomous delivery robots. They need sub-10ms response times for their real-time telemetry pipeline. Discussed our edge computing options and suggested our Rust SDK for their performance-critical paths.",
            "noteType": "meeting",
            "tags": ["sdk", "performance", "rust", "telemetry"],
            "createdAt": "2025-08-25T11:00:00Z"
        }),
        json!({
            "id": "n17",
            "customerId": "c5",
            "content": "Priya shared benchmark results from their POC. Our Rust SDK achieved 4ms P99 latency, well within their requirements. The Go SDK was at 12ms — acceptable for their monitoring dashboard but not for control loops. She wants to proceed with a hybrid approach.",
            "noteType": "email",
            "tags": ["benchmark", "rust", "go", "performance"],
            "createdAt": "2025-09-18T13:00:00Z"
        }),
        json!({
            "id": "n18",
            "customerId": "c5",
            "content": "Architecture review with Priya and NexGen's platform team. Proposed an event-driven design using our streaming API for robot telemetry and batch API for analytics. They want to handle 10K concurrent robot connections at launch, scaling to 100K. Recommended our startup growth plan with reserved capacity.",
            "noteType": "meeting",
            "tags": ["architecture", "scaling", "event-driven"],
            "createdAt": "2025-10-22T14:00:00Z"
        }),
    ];

    for note in &notes {
        let id = note["id"].as_str().unwrap();
        redis::cmd("JSON.SET")
            .arg(format!("note:{id}"))
            .arg("$")
            .arg(note.to_string())
            .query_async::<()>(&mut conn)
            .await?;
    }

    tracing::info!(
        customers = customers.len(),
        notes = notes.len(),
        "Seed data loaded"
    );

    Ok(())
}
