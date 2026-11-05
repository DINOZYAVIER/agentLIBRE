# Gemma 4: thinking in a channel and calls in its own language (AGL-050), as
# the template of the model writes them: arguments after a comma, text in
# `<|"|>` marks, integers bare. A step without thinking has an empty channel
# with no line in it, or nothing after a tool response: the model goes on there,
# and an empty channel makes it reason in the open. Only `${` interpolates in a Nix string: `{name}` and
# `{value}` are the format's own placeholders, and the other braces are text.
{
  think = {
    open = "<|channel>thought\n";
    close = "\n<channel|>";
    empty = "<|channel>thought\n<channel|>";
    no_block_after = "<tool_response|>";
    # It may reason in the open even so, and ends that with this mark. It may
    # also open its block by itself; the grammar then lets only `empty` through.
    stray_close = "<channel|>";
  };
  call = {
    begin = "<|tool_call>call:{name}{";
    param = "{name}:<|\"|>{value}<|\"|>";
    int_param = "{name}:{value}";
    separator = ",";
    # The text outside a call shows none of the model's marks. The opening ones
    # all start with `<|`; `stray_close` is not here, it is taken out afterwards.
    no_text = [ "<|" "<tool_call|>" "<tool_response|>" "<turn|>" ];
    # The template writes the text before a call after the response to it.
    text_in_history = false;
    end = "}<tool_call|>";
  };
}
