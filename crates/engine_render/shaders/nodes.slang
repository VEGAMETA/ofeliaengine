// The mark of a function the material graph shows as a node (ADR 0123's E10-3g-3).
//
//     #include "nodes.slang"
//
//     [Node("Swirl", "Mapping")]
//     float3 swirl(MaterialContext ctx, float3 color, float amount, out float mask)
//     {
//         ...
//     }
//
// A function of the project's Slang modules marked so is a node titled by the mark's first
// argument, found in the node search under its second. Its inputs are its parameters (a first
// MaterialContext is the fragment's, not an input); its outputs are what it returns (`out`)
// and its `out` and `inout` parameters. Every port is float to float4. A graph's module
// includes this file before the modules its calls are in, so a module used only by graphs
// works without including it; one also used by a hand-written material type includes it.
#pragma once

[__AttributeUsage(_AttributeTargets.Function)]
struct NodeAttribute
{
    string title;
    string category;
};
