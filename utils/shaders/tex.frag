#version 450
layout(set = 0, binding = 0) uniform sampler2D tex;
layout(push_constant) uniform Pc {
    vec4 rect;
    vec4 colour;
} pc;
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_colour;
void main() { out_colour = texture(tex, v_uv); }
