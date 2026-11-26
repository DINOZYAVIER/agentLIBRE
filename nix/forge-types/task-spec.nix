# The task-spec: a task of the project, from its spec to its commits
# (AGL-008, AGL-044, AGL-124). Its lifecycle is gated by the human.
{
  statuses = [ "open" "in-progress" "done" "backlog" "canceled" ];
  initial = "open";
  edges = {
    open = [ "in-progress" "backlog" "canceled" ];
    in-progress = [ "done" "backlog" "canceled" ];
    backlog = [ "in-progress" ];
  };
  fields = {
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
    "task-spec" = {
      kind = "range";
      required = true;
      default = { anchor = ""; last = ""; };
      description = "The git range of the work, anchor..last; `agl forge range` sets it.";
    };
    "verification.cmds" = {
      kind = "cmds";
      default = [ ];
      description = "The commands `agl forge check` runs.";
    };
  };
  body = [
    { heading = "Objective"; hint = "What this task changes, and its scope."; }
    { heading = "Decisions"; hint = "Settled choices: the decision, its date, and who set it."; }
    { heading = "Open questions"; hint = "Questions whose answers change code, stored data or behavior; none when there are none."; }
    { heading = "Changes"; hint = "The concrete changes: files, types and behavior."; }
    { heading = "Progress"; hint = "Append-only, newest last: dated bullets with commit refs."; appendOnly = true; }
    { heading = "Verification"; hint = "What proves the task is done."; }
  ];
  actions = {
    edit = [ "open" "in-progress" "backlog" ];
    append = [ "open" "in-progress" ];
    remove = [ "open" "canceled" "backlog" ];
  };
}
