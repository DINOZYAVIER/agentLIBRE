# DinoBot on E2B on the CPU, for 8 GB of RAM (AGL-050, D14-D16).
{ agl, ... }:
{
  policy = agl.policies.agentLIBRE;
  model = agl.models."gemma-4-e2b-qat" // {
    context = 32768;
    args = agl.args.cpu;
  };
}
