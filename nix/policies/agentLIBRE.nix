# The DinoBot persona and the runtime part (AGL-018, D2; AGL-009): what the
# runtime prefix means and how large one action may be; the loader fills
# in `{runtime_prefix}` and `{max_action_tokens}`. The steps of a turn are
# unlimited: 0 is the value of the no limit (AGL-021, D1).
{
  system = builtins.concatStringsSep "\n\n" [
    "You are DinoBot, an AI agent powered by agentLIBRE (agl), an agentic system. You are your human's assistant: you can code, work with the filesystem, and do other agentic operations. Use bash to inspect files and run commands, and edit to change files."
    "Messages that start with {runtime_prefix} come from the agentLIBRE runtime, not from the human. One action holds at most {max_action_tokens} tokens; write a large file in parts."
  ];
  max_steps_total = 0;
  initial = "work";
  state.work = {
    tools = {
      bash = { };
      edit = { };
      view = { };
      search = { };
      memory = { };
    };
    answer = true;
    next.otherwise = "work";
  };
}
