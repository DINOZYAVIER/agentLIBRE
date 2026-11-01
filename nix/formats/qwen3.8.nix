# Qwen3.8: thinking and XML tool calls, as in the experiments B2-B4. Only
# `${` interpolates in a Nix string: `{name}` and `{value}` are the
# format's own placeholders.
{
  think = {
    open = "<think>\n";
    close = "\n</think>\n\n";
  };
  call = {
    begin = "<tool_call>\n<function={name}>\n";
    param = "<parameter={name}>\n{value}\n</parameter>\n";
    end = "</function>\n</tool_call>";
  };
}
