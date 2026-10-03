/** Per-run reservation state. States are replaced, never mutated in place. */

export type RunState =
  | { readonly status: "reserved"; readonly reservationId: string; readonly at: number }
  | { readonly status: "reconciled"; readonly reservationId: string; readonly at: number }
  | { readonly status: "refused"; readonly why: string; readonly at: number };

const STATE_CAP = 2_000;
const STATE_TTL_MS = 60 * 60 * 1_000;

export type RunStore = {
  readonly get: (runId: string) => RunState | undefined;
  readonly set: (runId: string, state: RunState) => void;
};

/** Bounded, insertion-ordered store: oldest and expired entries are evicted on every write. */
export function createRunStore(now: () => number): RunStore {
  const runs = new Map<string, RunState>();
  return {
    get: (runId) => runs.get(runId),
    set: (runId, state) => {
      runs.delete(runId);
      runs.set(runId, state);
      for (const [key, value] of runs) {
        if (runs.size <= STATE_CAP && now() - value.at <= STATE_TTL_MS) break;
        runs.delete(key);
      }
    },
  };
}
