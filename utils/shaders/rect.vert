#version 450
// 4 vertices (triangle strip) placed from the push-constant rect, in NDC.
// pc.colour.z carries the depth so a scene can place quads at chosen z.
layout(push_constant) uniform Pc {
    vec4 rect;    // x, y, w, h
    vec4 colour;  // rgb + alpha, z = depth
} pc;
layout(location = 0) out vec2 v_uv;
void main() {
    vec2 c = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    v_uv = c;
    gl_Position = vec4(pc.rect.xy + c * pc.rect.zw, pc.colour.z, 1.0);
}
