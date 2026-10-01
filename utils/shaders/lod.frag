#version 450
// Sample an explicit mip level: pc.colour.x is the LOD, so a scene can read
// level 0 and level 1 side by side.
layout(set = 0, binding = 0) uniform sampler2D tex;
layout(push_constant) uniform Pc {
    vec4 rect;
    vec4 colour;
} pc;
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_colour;
void main() {
    vec4 s = textureLod(tex, v_uv, pc.colour.x);
    out_colour = vec4(s.rgb, 1.0);
}
