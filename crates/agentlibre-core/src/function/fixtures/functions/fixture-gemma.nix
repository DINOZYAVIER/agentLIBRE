# The tests' Function in the Gemma format (AGL-053, D8).
{ agl, ... }:
{
  policy = agl.policies.agentLIBRE;
  model = agl.models."gemma-4-e4b-qat";
}
