#version 450
layout(push_constant) uniform Pc {
    vec4 rect;
    vec4 colour;
} pc;
layout(location = 0) out vec4 out_colour;
void main() { out_colour = pc.colour; }
