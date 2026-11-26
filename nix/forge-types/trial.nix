# The trial: a new model or another outside thing brought up on our
# machine, measured and patched locally (AGL-124, D1–D5, D10, D14, D21).
{
  statuses = [ "open" "running" "concluded" "dropped" ];
  initial = "open";
  edges = {
    open = [ "running" "dropped" ];
    running = [ "concluded" "dropped" ];
  };
  fields = {
    subject = {
      kind = "string";
      required = true;
      default = "";
      description = "What is tried: the model and its source.";
    };
    rig = {
      kind = "string";
      required = true;
      default = "";
      description = "The machine it runs on: GPU, RAM, OS.";
    };
    branch = {
      kind = "string";
      required = true;
      default = "";
      description = "The branch or worktree that holds the local patches.";
    };
    deps = {
      kind = "refs";
      default = [ ];
      description = "The artifacts this one waits for.";
    };
    supersedes = {
      kind = "refs";
      default = [ ];
      description = "The artifacts this one replaces.";
    };
    "verification.cmds" = {
      kind = "cmds";
      default = [ ];
      description = "The commands `agl forge check` runs.";
    };
  };
  body = [
    { heading = "Goal"; hint = "What is tried and what would count as a success."; }
    { heading = "Log"; hint = "Append-only, newest last: dated bullets, what was done and what broke, with evidence."; appendOnly = true; }
    { heading = "Measurements"; hint = "The bench command, where the raw results are, the table."; }
    { heading = "Patches"; hint = "Each local change: where, the diff or commit, and why."; }
    { heading = "Conclusion"; hint = "Adopt, park or drop, and why."; }
    { heading = "Open questions"; hint = "What is still unknown."; }
  ];
  actions = {
    edit = [ "open" "running" ];
    append = [ "open" "running" ];
    remove = [ "open" "dropped" ];
  };
  prefix = "trial";
}
