import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { PLUGIN_ID, registerPair } from "./register.ts";

export default definePluginEntry({
  id: PLUGIN_ID,
  name: "PAIR Spike",
  description: "PAIR adapter: policy-gated tools, budget-gated model calls, reconciled usage",
  register(api) {
    registerPair(api, process.env);
  },
});
