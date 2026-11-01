# DinoBot on the default model, the Function a run loads when the settings
# name none (AGL-039, D28, D32, D44).
{ agl, ... }:
{
  policy = agl.policies.agentLIBRE;
  model = agl.models."qwen3.8-27b-iq4_xs";
}
