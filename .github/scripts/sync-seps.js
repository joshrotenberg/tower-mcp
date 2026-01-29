/**
 * SEP (Specification Enhancement Proposal) Sync Script
 *
 * Syncs SEP tracking issues from the upstream MCP specification repository.
 * Run via GitHub Actions workflow or locally for testing.
 *
 * @param {Object} params - GitHub Actions context
 * @param {Object} params.github - Octokit REST client
 * @param {Object} params.context - GitHub Actions context
 * @param {Object} params.core - GitHub Actions core utilities
 */
module.exports = async ({ github, context, core }) => {
  const UPSTREAM_REPO = 'modelcontextprotocol/specification';

  // Map upstream labels to our status labels
  const STATUS_MAP = {
    'draft': 'sep:draft',
    'proposal': 'sep:proposal',
    'in-review': 'sep:in-review',
    'accepted': 'sep:accepted',
    'accepted-with-changes': 'sep:accepted',
    'final': 'sep:final',
    'dormant': 'sep:dormant',
    'rejected': 'sep:rejected',
  };

  // Fetch all SEPs from upstream
  console.log('Fetching SEPs from upstream...');
  const upstreamIssues = await github.paginate(github.rest.issues.listForRepo, {
    owner: 'modelcontextprotocol',
    repo: 'specification',
    labels: 'SEP',
    state: 'all',
    per_page: 100,
  });

  console.log(`Found ${upstreamIssues.length} SEPs upstream`);

  // Fetch our existing SEP tracking issues
  const ourIssues = await github.paginate(github.rest.issues.listForRepo, {
    owner: context.repo.owner,
    repo: context.repo.repo,
    labels: 'sep',
    state: 'all',
    per_page: 100,
  });

  console.log(`Found ${ourIssues.length} SEP tracking issues locally`);

  // Build a map of SEP number -> our issue
  const sepToIssue = new Map();
  for (const issue of ourIssues) {
    // Extract SEP number from title (e.g., "SEP-1699" or "[final] SEP-1699: ...")
    const match = issue.title.match(/SEP-(\d+)/i);
    if (match) {
      sepToIssue.set(match[1], issue);
    }
  }

  // Track stats
  let created = 0;
  let updated = 0;
  let skipped = 0;

  // Process each upstream SEP
  for (const sep of upstreamIssues) {
    const sepMatch = sep.title.match(/SEP-(\d+)/i);
    if (!sepMatch) continue;

    const sepNumber = sepMatch[1];
    const sepTitle = sep.title.replace(/^SEP-\d+:\s*/, '').trim();
    const upstreamLabels = sep.labels.map(l => l.name);

    // Determine status from upstream labels
    let status = 'sep:proposal'; // default
    for (const [upstreamLabel, ourLabel] of Object.entries(STATUS_MAP)) {
      if (upstreamLabels.includes(upstreamLabel)) {
        status = ourLabel;
        break;
      }
    }

    // Check if SEP is closed/final upstream
    const isFinal = sep.state === 'closed' || upstreamLabels.includes('final');
    if (isFinal) {
      status = 'sep:final';
    }
    const statusPrefix = isFinal ? '[final]' :
                         status === 'sep:accepted' ? '[accepted]' :
                         status === 'sep:in-review' ? '[in-review]' :
                         status === 'sep:draft' ? '[draft]' :
                         status === 'sep:dormant' ? '[dormant]' :
                         status === 'sep:rejected' ? '[rejected]' :
                         '[proposal]';

    const newTitle = `${statusPrefix} SEP-${sepNumber}: ${sepTitle}`;

    if (sepToIssue.has(sepNumber)) {
      // Update existing issue
      const existing = sepToIssue.get(sepNumber);
      const existingLabels = existing.labels.map(l => l.name);

      // Skip closed issues or issues marked as implemented/not-applicable
      if (existing.state === 'closed') {
        skipped++;
        continue;
      }
      if (existingLabels.includes('implemented') || existingLabels.includes('not-applicable')) {
        console.log(`Skipping SEP-${sepNumber}: marked as implemented or not-applicable`);
        skipped++;
        continue;
      }

      // Check if we need to update
      const needsTitleUpdate = existing.title !== newTitle;
      const needsLabelUpdate = !existingLabels.includes(status);

      if (needsTitleUpdate || needsLabelUpdate) {
        console.log(`Updating SEP-${sepNumber}: ${existing.title} -> ${newTitle}`);

        // Remove old status labels, add new one
        const newLabels = existingLabels
          .filter(l => !l.startsWith('sep:'))
          .concat(['sep', status]);

        await github.rest.issues.update({
          owner: context.repo.owner,
          repo: context.repo.repo,
          issue_number: existing.number,
          title: newTitle,
          labels: newLabels,
        });
        updated++;
      }
    } else {
      // Create new issue
      console.log(`Creating new issue for SEP-${sepNumber}: ${sepTitle}`);

      // Build body with array join to avoid YAML parsing issues
      const bodyLines = [
        `## SEP-${sepNumber}: ${sepTitle}`,
        '',
        `**Upstream Issue:** https://github.com/${UPSTREAM_REPO}/issues/${sep.number}`,
        `**Status:** ${statusPrefix.replace(/[\[\]]/g, '')}`,
        `**Upstream State:** ${sep.state}`,
        '',
        '### Description',
        '',
        sep.body ? sep.body.slice(0, 500) + (sep.body.length > 500 ? '...' : '') : 'See upstream issue for details.',
        '',
        '### Implementation Status',
        '',
        '- [ ] Researched/understood the SEP',
        '- [ ] Determined relevance to tower-mcp',
        '- [ ] Implementation planned',
        '- [ ] Implementation complete',
        '- [ ] Tests added',
        '- [ ] Documentation updated',
        '',
        '---',
        '_This issue is automatically synced from the MCP specification repository._',
        `_Last synced: ${new Date().toISOString().split('T')[0]}_`,
      ];
      const body = bodyLines.join('\n');

      await github.rest.issues.create({
        owner: context.repo.owner,
        repo: context.repo.repo,
        title: newTitle,
        body: body,
        labels: ['sep', status],
      });
      created++;
    }
  }

  console.log(`SEP sync complete! Created: ${created}, Updated: ${updated}, Skipped: ${skipped}`);
};
