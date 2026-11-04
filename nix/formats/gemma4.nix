# Gemma 4: thinking in a channel and calls in its own language (AGL-050), as
# the template of the model writes them: arguments after a comma, text in
# `<|"|>` marks, integers bare. A step without thinking has an empty channel
# with no line in it. Only `${` interpolates in a Nix string: `{name}` and
# `{value}` are the format's own placeholders, and the other braces are text.
{
  think = {
    open = "<|channel>thought\n";
    close = "\n<channel|>";
    empty = "<|channel>thought\n<channel|>";
  };
  call = {
    begin = "<|tool_call>call:{name}{";
    param = "{name}:<|\"|>{value}<|\"|>";
    int_param = "{name}:{value}";
    separator = ",";
    end = "}<tool_call|>";
  };
}
