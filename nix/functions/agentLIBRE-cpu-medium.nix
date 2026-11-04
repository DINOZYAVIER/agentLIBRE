# DinoBot on E4B on the CPU, from 16 GB of RAM (AGL-050, D14-D16).
{ agl, ... }:
{
  policy = agl.policies.agentLIBRE;
  model = agl.models."gemma-4-e4b-qat" // {
    context = 32768;
    args = agl.args.cpu;
  };
}
