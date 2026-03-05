/**
 * SEP (Specification Enhancement Proposal) Sync Script
 *
 * Syncs SEP tracking issues from the upstream MCP specification repository.
 * Run via GitHub Actions workflow (.github/workflows/sep-sync.yml) or manually.
 *
 * The script:
 * 1. Fetches all issues with the "SEP" label from upstream
 * 2. Matches them against our existing spec-tracking issues by SEP number
 * 3. Creates new tracking issues for untracked SEPs
 * 4. Updates title prefixes when upstream status changes
 * 5. Skips closed issues (already implemented or not applicable)
 *
 * @param {Object} params - GitHub Actions context
 * @param {Object} params.github - Octokit REST client
 * @param {Object} params.context - GitHub Actions context
 * @param {Object} params.core - GitHub Actions core utilities
 */
module.exports = async ({ github, context, core }) => {
  const UPSTREAM_OWNER = 'modelcontextprotocol';
  const UPSTREAM_REPO = 'modelcontextprotocol';

  // Map upstream labels to title prefixes
  const STATUS_PREFIX = {
    'accepted': '[accepted]',
    'accepted-with-changes': '[accepted]',
    'final': '[final]',
    'in-review': '[in-review]',
    'draft': '[draft]',
    'dormant': '[dormant]',
    'rejected': '[rejected]',
    'proposal': '[proposal]',
  };

  // Fetch all SEP-labeled issues from upstream
  core.info('Fetching SEPs from upstream...');
  const upstreamIssues = await github.paginate(github.rest.issues.listForRepo, {
    owner: UPSTREAM_OWNER,
    repo: UPSTREAM_REPO,
    labels: 'SEP',
    state: 'all',
    per_page: 100,
  });

  core.info(`Found ${upstreamIssues.length} SEPs upstream`);

  // Fetch our existing spec-tracking issues (both labels for backwards compat)
  const ourIssues = await github.paginate(github.rest.issues.listForRepo, {
    owner: context.repo.owner,
    repo: context.repo.repo,
    labels: 'spec-tracking',
    state: 'all',
    per_page: 100,
  });

  core.info(`Found ${ourIssues.length} spec-tracking issues locally`);

  // Build a map of SEP number -> our issue
  const sepToIssue = new Map();
  for (const issue of ourIssues) {
    const match = issue.title.match(/SEP-(\d+)/i);
    if (match) {
      sepToIssue.set(match[1], issue);
    }
  }

  let created = 0;
  let updated = 0;
  let skipped = 0;

  for (const sep of upstreamIssues) {
    const sepMatch = sep.title.match(/SEP-(\d+)/i);
    if (!sepMatch) continue;

    const sepNumber = sepMatch[1];
    const sepTitle = sep.title.replace(/^SEP-\d+[:\s]*/, '').trim();
    const upstreamLabels = sep.labels.map(l => l.name);

    // Determine status prefix from upstream labels
    let prefix = '[proposal]';
    for (const [label, pfx] of Object.entries(STATUS_PREFIX)) {
      if (upstreamLabels.includes(label)) {
        prefix = pfx;
        break;
      }
    }

    // Closed + accepted upstream = final
    if (sep.state === 'closed' && (prefix === '[accepted]' || upstreamLabels.includes('final'))) {
      prefix = '[accepted]';
    }

    const newTitle = `${prefix} SEP-${sepNumber}: ${sepTitle}`;

    if (sepToIssue.has(sepNumber)) {
      const existing = sepToIssue.get(sepNumber);

      // Skip closed issues (implemented, not-applicable, etc.)
      if (existing.state === 'closed') {
        skipped++;
        continue;
      }

      // Update title if status prefix changed
      if (existing.title !== newTitle) {
        core.info(`Updating SEP-${sepNumber}: ${existing.title} -> ${newTitle}`);
        await github.rest.issues.update({
          owner: context.repo.owner,
          repo: context.repo.repo,
          issue_number: existing.number,
          title: newTitle,
        });
        updated++;
      } else {
        skipped++;
      }
    } else {
      // Create new tracking issue
      core.info(`Creating issue for SEP-${sepNumber}: ${sepTitle}`);

      const body = [
        `## SEP-${sepNumber}: ${sepTitle}`,
        '',
        `**Upstream:** https://github.com/${UPSTREAM_OWNER}/${UPSTREAM_REPO}/issues/${sep.number}`,
        `**Status:** ${prefix.replace(/[\[\]]/g, '')}`,
        '',
        '### Description',
        '',
        sep.body
          ? sep.body.slice(0, 500) + (sep.body.length > 500 ? '...' : '')
          : 'See upstream issue for details.',
        '',
        '### Action Items',
        '',
        '- [ ] Review the SEP and determine relevance to tower-mcp',
        '- [ ] Assess implementation scope (if applicable)',
        '- [ ] Implement or close as not-applicable',
        '',
        '---',
        '_Auto-synced from the MCP specification repository._',
        `_Last synced: ${new Date().toISOString().split('T')[0]}_`,
      ].join('\n');

      await github.rest.issues.create({
        owner: context.repo.owner,
        repo: context.repo.repo,
        title: newTitle,
        body: body,
        labels: ['spec-tracking', 'enhancement'],
      });
      created++;
    }
  }

  core.info(`SEP sync complete. Created: ${created}, Updated: ${updated}, Skipped: ${skipped}`);

  // Set outputs for workflow summary
  core.setOutput('created', created);
  core.setOutput('updated', updated);
  core.setOutput('skipped', skipped);
  core.setOutput('total_upstream', upstreamIssues.length);
};
