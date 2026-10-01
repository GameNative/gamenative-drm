#version 450
// Procedural 2x2 checker from the interpolated uv: no sampler involved.
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_colour;
void main() {
    vec2 g = floor(v_uv * 2.0);
    float c = mod(g.x + g.y, 2.0);
    out_colour = vec4(vec3(c), 1.0);
}
