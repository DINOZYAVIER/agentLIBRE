# The tests' Function with the laconic policy, on the same model (AGL-053, D8).
{ agl, ... }:
{
  policy = agl.policies.minimal;
  model = agl.models."qwen3.8-27b-iq4_xs";
}
