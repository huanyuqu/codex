export const message = (text, id, role = "user") => ({
  type: "message",
  id,
  role,
  content: [{ type: role === "user" ? "input_text" : "output_text", text }],
});

// A controlled model response for protocol tests; live synthesis is validated separately.
export function fixtureState(archive, goal) {
  const users = archive.records.filter((r) => {
    const item = JSON.parse(r.json);
    return item.type === "message" && item.role === "user";
  });
  const results = archive.records.filter(
    (r) => JSON.parse(r.json).type === "function_call_output",
  );
  return {
    goal: goal ?? "Add caching without changing the API",
    goalRefs: users.slice(0, 1).map((r) => r.ref),
    constraints: users.map((r) => ({
      text: JSON.parse(r.json).content[0].text,
      refs: [r.ref],
    })),
    facts: results.map((r) => ({
      text: String(JSON.parse(r.json).output),
      refs: [r.ref],
      status: "verified",
      scope: "Historical test in " + r.sourceThreadId,
    })),
    decisions: [],
    completed: [],
    pending: results.slice(-1).map((r) => ({
      text: "Investigate the failing cache invalidation test",
      refs: [r.ref],
    })),
    nextActions: [
      {
        text: "Fix stale cache data, then rerun performance and correctness tests",
        refs: users.map((r) => r.ref),
      },
    ],
    conflicts:
      results.length < 2
        ? []
        : [
            {
              topic: "Cache correctness",
              alternatives: results.slice(0, 2).map((r) => ({
                text: String(JSON.parse(r.json).output),
                refs: [r.ref],
                scope: "Historical result in " + r.sourceThreadId,
                status: "verified",
              })),
              status: "unresolved",
              resolution: "",
              resolutionRefs: [],
            },
          ],
  };
}
