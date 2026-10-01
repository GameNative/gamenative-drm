#version 450
// texelFetch bypasses the sampler: validates the image view, descriptor and
// upload without any filtering/coordinate machinery.
layout(set = 0, binding = 0) uniform sampler2D tex;
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_colour;
void main() {
    ivec2 t = clamp(ivec2(floor(v_uv * 2.0)), ivec2(0), ivec2(1));
    out_colour = texelFetch(tex, t, 0);
}
