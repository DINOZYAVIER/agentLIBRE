# DinoBot on E4B on the CPU with all its context, on a fast CPU: a full read takes half an hour at 80 tokens/s (AGL-050, D14-D16).
{ agl, ... }:
{
  policy = agl.policies.agentLIBRE;
  model = agl.models."gemma-4-e4b-qat" // {
    context = 131072;
    args = agl.args.cpu;
  };
}
