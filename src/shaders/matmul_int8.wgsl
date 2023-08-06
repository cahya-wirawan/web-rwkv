@group(0) @binding(0) var<uniform> dims: vec2<u32>;                         // [C, R]
@group(0) @binding(1) var<uniform> num_tokens: u32;

@group(0) @binding(2) var<storage, read> matrix: array<u32>;                // (R, C)
@group(0) @binding(3) var<storage, read> mx: array<vec4<f32>>;              // (C)
@group(0) @binding(4) var<storage, read> rx: array<vec4<f32>>;              // (C)
@group(0) @binding(5) var<storage, read> my: array<vec4<f32>>;              // (R)
@group(0) @binding(6) var<storage, read> ry: array<vec4<f32>>;              // (R)

@group(0) @binding(7) var<storage, read> input: array<vec4<f32>>;           // (T, C)
@group(0) @binding(8) var<storage, read_write> output: array<vec4<f32>>;    // (T, R)

const BLOCK_SIZE: u32 = 128u;

var<workgroup> sketch: array<vec4<f32>, BLOCK_SIZE>;

fn reduce_sum(index: u32, stride: u32) {
    if index < stride {
        sketch[index] += sketch[index + stride];
    }
    workgroupBarrier();
}

@compute @workgroup_size(128, 1, 1)
fn matmul(@builtin(global_invocation_id) invocation_id: vec3<u32>) {
    let index = invocation_id.x;
    let channel = invocation_id.y;      // 1 channel: 4 rows in matrix
    let token = invocation_id.z;
    let stride = dims / 4u;

    if token >= num_tokens {
        return;
    }

    let myc = my[channel];
    let ryc = ry[channel];

    var local_sum = vec4<f32>(0.0);
    for (var i = index; i < stride.x; i += BLOCK_SIZE) {
        let ti = token * stride.x + i;
        var ci = channel * 4u * stride.x + i;

        // read 4 elements from the input
        let x = input[ti];

        let mxi = mx[i];
        let rxi = rx[i];

        // read 4 rows from the matrix, each with 4 unpacked floats, forming a 4x4 sub-block
        var m: mat4x4<f32>;

        m[0] = fma(unpack4x8unorm(matrix[ci]), ryc[0] * rxi, myc[0] + mxi); ci += stride.x;
        m[1] = fma(unpack4x8unorm(matrix[ci]), ryc[1] * rxi, myc[1] + mxi); ci += stride.x;
        m[2] = fma(unpack4x8unorm(matrix[ci]), ryc[2] * rxi, myc[2] + mxi); ci += stride.x;
        m[3] = fma(unpack4x8unorm(matrix[ci]), ryc[3] * rxi, myc[3] + mxi);
        local_sum += transpose(m) * x;
    }
    sketch[index] = local_sum;
    workgroupBarrier();

    reduce_sum(index, 64u);
    reduce_sum(index, 32u);
    reduce_sum(index, 16u);
    reduce_sum(index, 8u);
    reduce_sum(index, 4u);
    reduce_sum(index, 2u);
    reduce_sum(index, 1u);

    if index == 0u {
        output[token * stride.y + channel] = sketch[0];
    }
}