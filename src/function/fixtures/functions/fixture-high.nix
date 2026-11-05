# The tests' Function with the full policy (AGL-053, D8).
{ agl, ... }:
{
  policy = agl.policies.agentLIBRE;
  model = agl.models."qwen3.8-27b-iq4_xs";
}
