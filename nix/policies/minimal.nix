# The laconic nameless policy (AGL-018): one sentence about agentLIBRE, no
# name and no runtime part; the steps of a turn are limited to the default
# 50 (D40). The states are those of `agentLIBRE`.
{
  system = "You are an AI agent running on agentLIBRE (agl), an agentic system.";
  initial = "inference";
  # The one state for now; other states and the transitions to them come
  # later (AGL-046, D7).
  state.inference = {
    tools = {
      bash = { };
      edit = { };
      view = { };
      search = { };
      memory = { };
    };
    answer = true;
    next.otherwise = "inference";
  };
}
