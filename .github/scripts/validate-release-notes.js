/**
 * Validates release notes in PR body
 * Sets outputs for label and validation status
 */

const pr = context.payload.pull_request;
const body = pr.body || '';

// Parse release-note block
const releaseNoteMatch = body.match(/```release-note\s*([\s\S]*?)\s*```/);
const hasReleaseNoteBlock = !!releaseNoteMatch;
const releaseNoteContent = releaseNoteMatch ? releaseNoteMatch[1].trim() : '';

console.log('PR:', pr.number);
console.log('Has release-note block:', hasReleaseNoteBlock);
console.log('Content:', releaseNoteContent);

if (!hasReleaseNoteBlock || releaseNoteContent === '') {
  console.log('❌ No release-note block found or empty');
  core.setOutput('label', 'release-note/invalid');
  core.setOutput('valid', 'false');
  core.setFailed('Release note block is missing or empty');
  process.exit(1);
}

// Validate content
const isNone = releaseNoteContent === 'NONE';
const isEmpty = releaseNoteContent.length === 0;
const isTooShort = releaseNoteContent.length < 10;

if (isNone) {
  console.log('✓ Release note: NONE (no user-facing changes)');
  core.setOutput('label', 'release-note/none');
  core.setOutput('valid', 'true');
  core.setOutput('content', 'NONE');
  process.exit(0);
}

if (isEmpty || isTooShort) {
  console.log('❌ Release note is too short or empty');
  core.setOutput('label', 'release-note/invalid');
  core.setOutput('valid', 'false');
  core.setFailed('Release note must be meaningful or explicitly "NONE"');
  process.exit(1);
}

// Check for meaningful content (not just placeholders)
const placeholders = ['<your release note', 'TODO', '...', '[description]'];
const hasPlaceholder = placeholders.some(p => releaseNoteContent.toLowerCase().includes(p.toLowerCase()));

if (hasPlaceholder) {
  console.log('❌ Release note contains placeholder text');
  core.setOutput('label', 'release-note/invalid');
  core.setOutput('valid', 'false');
  core.setFailed('Release note contains placeholder text - please provide actual content');
  process.exit(1);
}

// All checks passed
console.log('✓ Release note is valid');
console.log(`Content: "${releaseNoteContent}"`);
core.setOutput('label', 'release-note');
core.setOutput('valid', 'true');
core.setOutput('content', releaseNoteContent);
