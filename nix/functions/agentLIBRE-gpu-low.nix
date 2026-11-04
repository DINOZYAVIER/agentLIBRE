# DinoBot on E4B on the GPU, from 6 GB of VRAM (AGL-050, D14-D16).
{ agl, ... }:
{
  policy = agl.policies.agentLIBRE;
  model = agl.models."gemma-4-e4b-qat";
}
