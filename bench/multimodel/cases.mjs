// Cases for the multi-model benchmark.
//
// router: drives the production `route_request` (d1 decision + keyword
//         fallback) and records both routes for comparison.
// audio:  drives LFM3-Audio-2B synthesis and transcription directly.
// chat:   drives the production chat loop, tools, approvals, and the vision
//         and audio tools. Content checks run externally in run.mjs.

const visionNumber = "assets/vision-number.png";
const visionShapes = "assets/vision-shapes.png";

export const router = [
  {
    id: "r01-look-like-vision",
    prompt: "Could you take a look at notes/coffee.md and tell me how often the water filter is changed?",
    expected: "summarize",
    note: "The keyword 'look at' tempts the heuristic into vision; the request is a file summary.",
  },
  {
    id: "r02-look-vendor",
    prompt: "Can you look at the vendor email and pull out the order number?",
    expected: "summarize",
    note: "Same trap, short request.",
  },
  {
    id: "r03-what-does-policy",
    prompt: "What does the refund policy say about shipping fees?",
    expected: "summarize",
    note: "Control: keyword route already right.",
  },
  {
    id: "r04-brief-incident",
    prompt: "I need a short brief of the incident note before standup.",
    expected: "summarize",
    note: "No keyword group fires; d1 must recover the route.",
  },
  {
    id: "r05-find-water-filter",
    prompt: "Find the file that mentions the water filter.",
    expected: "explore",
    note: "Control: explicit find.",
  },
  {
    id: "r06-connected-greenhouse",
    prompt: "Which notes are connected to the greenhouse plan?",
    expected: "related",
    note: "Control: explicit related vocabulary.",
  },
  {
    id: "r07-deliverable-policy",
    prompt: "Please prepare a deliverable about the expense policy.",
    expected: "report",
    note: "Control: explicit deliverable.",
  },
  {
    id: "r08-recording",
    prompt: "There is a recording of the budget review; tell me what was said.",
    expected: "voice",
    note: "Control: explicit recording.",
  },
  {
    id: "r09-picture",
    prompt: "What number is printed in the picture on the desk?",
    expected: "vision",
    note: "Control: explicit picture.",
  },
  {
    id: "r10-analyse-folder",
    prompt: "Analyse the notes folder and tell me which topics it covers.",
    expected: "explore",
    note: "Control: explicit folder analysis.",
  },
  {
    id: "r11-figure-out-gift-cards",
    prompt: "Can you figure out what the policy file says about gift cards?",
    expected: "summarize",
    note: "'figure' tempts the heuristic into vision; the request reads one named file.",
  },
  {
    id: "r12-explain-expenses",
    prompt: "Please read data/expenses.csv and explain this file to me.",
    expected: "summarize",
    note: "Control: explicit read and explain.",
  },
  {
    id: "r13-where-onboarding",
    prompt: "Where is the onboarding checklist kept?",
    expected: "explore",
    note: "Control: explicit location question.",
  },
  {
    id: "r14-which-note",
    prompt: "Which note mentions both water and a machine?",
    expected: "explore",
    note: "Task-shaped but no keyword group fires; d1 must recover the route.",
  },
  {
    id: "r15-hello",
    prompt: "Hello there, how are you doing today?",
    expected: "general",
    note: "Control: plain conversation.",
  },
  {
    id: "r16-summarise-meeting",
    prompt: "Summarise the product meeting notes in three bullets.",
    expected: "summarize",
    note: "Control: explicit summary.",
  },
  {
    id: "r17-list-docs",
    prompt: "List every file in the docs folder.",
    expected: "explore",
    note: "Control: explicit list.",
  },
  {
    id: "r18-draft-report",
    prompt: "Draft a report on the inventory levels for the team.",
    expected: "report",
    note: "Control: explicit report.",
  },
  {
    id: "r19-take-a-look-meeting",
    prompt: "Take a look at docs/meeting.md and tell me when the beta launches.",
    expected: "summarize",
    note: "Again 'look at' points at vision, but this is a single-file lookup.",
  },
  {
    id: "r20-gist-vendor",
    prompt: "Give me the gist of the vendor email.",
    expected: "summarize",
    note: "No keyword group fires; d1 must recover the route.",
  },
  {
    id: "r21-transcribe-recording",
    prompt: "Transcribe the recording in the assets folder and read it back.",
    expected: "voice",
    note: "Control: explicit transcription.",
  },
];

export const audio = [
  {
    id: "a01-budget",
    text: "The quarterly budget review is scheduled for Thursday at three.",
    expect: ["thursday", "three"],
  },
  {
    id: "a02-tomatoes",
    text: "Please water the tomatoes every two days.",
    expect: ["water", "tomatoes"],
  },
];

export const chat = [
  {
    id: "c01-hello",
    prompt: "Hi! In one short sentence, what can you help me with in this workspace?",
    expect: { route: "general", contains: [/.{10,}/], tools: "none", approval: "none" },
  },
  {
    id: "c02-list-notes",
    prompt: "List the files in the notes folder and name one topic from each.",
    expect: {
      route: "explore",
      contains: [/coffee/i, /greenhouse/i],
      tools: ["list_files"],
      approval: "none",
    },
  },
  {
    id: "c03-greenhouse",
    prompt: "Read notes/greenhouse.md and tell me how often the tomatoes are watered.",
    expect: {
      route: "summarize",
      contains: [/every 2 days|every two days|2 days/i],
      tools: ["read_file"],
      approval: "none",
    },
  },
  {
    id: "c04-related",
    prompt: "Which files are related to notes/greenhouse.md, and why?",
    expect: {
      route: "related",
      contains: [/notes\/|\.md|csv|[a-z]+\.md/i],
      tools: ["related_files"],
      approval: "none",
    },
  },
  {
    id: "c05-report-allowed",
    prompt:
      "Write a two-sentence report to out/bench-report.md summarising the refund policy, including the 30-day window.",
    approve: "allow",
    expect: {
      route: "report",
      approval: "allowed",
      file: { path: "out/bench-report.md", contains: [/30 days?|thirty days?/i] },
      contains: [/refund/i],
    },
  },
  {
    id: "c06-write-denied",
    prompt: "Create out/denied.md containing the single word hello.",
    approve: "deny",
    expect: {
      approval: "denied",
      fileAbsent: "out/denied.md",
      contains: [/cannot|can't|denied|not able|without|permission|approval/i],
    },
  },
  {
    id: "c07-vision-number",
    prompt:
      "Inspect the attached picture at {image} and tell me the two-digit number printed in it.",
    image: visionNumber,
    expect: {
      route: "vision",
      contains: [/\b42\b/],
      tools: ["look"],
      approval: "none",
    },
  },
  {
    id: "c08-vision-shapes",
    prompt:
      "Look at the attached image at {image}. Which object is red: the circle or the square?",
    image: visionShapes,
    expect: {
      route: "vision",
      contains: [/\bcircle\b/i],
      tools: ["look"],
      approval: "none",
    },
  },
  {
    id: "c09-voice",
    prompt:
      "Transcribe the audio file at {audio} and tell me the day and time of the budget review.",
    audio: "a01-budget",
    expect: {
      route: "voice",
      contains: [/thursday/i, /\bthree\b|\b3\b/i],
      tools: ["transcribe"],
      approval: "none",
    },
  },
  {
    id: "c10-memory-set",
    session: "memory",
    prompt: "Remember that the product meeting moved to Friday.",
    expect: { contains: [/friday/i] },
  },
  {
    id: "c10-memory-recall",
    session: "memory",
    prompt: "What day did I say the product meeting moved to?",
    expect: { contains: [/friday/i] },
  },
  {
    id: "c11-missing-file",
    prompt: "Read notes/does-not-exist.md and summarise it.",
    expect: {
      contains: [
        /no file|there(?:'s| is) no\b|not exist|does(?:n't| not) exist|not (?:found|present)|could not find|couldn't find|no such|unable|missing/i,
      ],
      absent: [/water filter|tomatoes|vendor/i],
    },
  },
];
