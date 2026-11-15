//! Skills extension (SEP-2640) fixture.
//!
//! Serves two listed skills and one unlisted skill so the official
//! `sep-2640-skills-*` server scenarios have something to enumerate, verify,
//! and browse: a skill with supporting files and a nested subdirectory (for
//! the directory-read checks), a skill under an organizational prefix, and a
//! skill reachable only through `skills/get`.

use tower_mcp::{SkillBuilder, SkillError, SkillSet};

const PDF_SKILL_MD: &str = "---
name: pdf-processing
description: Extract, fill, and assemble PDF documents
license: Apache-2.0
metadata:
  author: conformance-server
---

# PDF processing

Choose the matching template from `templates/`, then follow
`references/FORMS.md` to fill it in.
";

const REFUNDS_SKILL_MD: &str = "---
name: refunds
description: Process customer refund requests per company policy
---

# Refunds

Use `examples/email.md` as the reply template.
";

const UNLISTED_SKILL_MD: &str = "---
name: chargebacks
description: Respond to card chargebacks
---

# Chargebacks

This skill is served by skills/get but left out of skills/list.
";

pub fn build_skills() -> Result<SkillSet, SkillError> {
    Ok(SkillSet::new()
        .skill(
            SkillBuilder::new("pdf-processing")
                .skill_md(PDF_SKILL_MD)
                .file("references/FORMS.md", "# Forms\n\nFill every field.\n")
                .file("templates/invoice.md", "# Invoice\n\nCustomer:\nAmount:\n")
                .file(
                    "templates/purchase-order.md",
                    "# Purchase order\n\nSupplier:\nItems:\n",
                )
                .file("templates/regional/eu-invoice.md", "# EU invoice\n\nVAT:\n"),
        )?
        .skill(
            SkillBuilder::new("acme/billing/refunds")
                .skill_md(REFUNDS_SKILL_MD)
                .file(
                    "examples/email.md",
                    "Dear customer,\n\nYour refund is on its way.\n",
                ),
        )?
        .skill(
            SkillBuilder::new("acme/billing/chargebacks")
                .skill_md(UNLISTED_SKILL_MD)
                .unlisted(),
        )?
        .directory_read(true))
}
