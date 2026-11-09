// The backdrop: a field of PCM samples drawn as a stem plot — the way digital audio actually
// looks before a DAC turns it back into a wave. Each row is a channel/moment, each stem one sample.
// Everything moves in the vertex shader, so the CPU only updates a handful of uniforms per frame.

import * as THREE from "three";

/** Everything the scroll timeline animates. Plain numbers so GSAP can tween them directly. */
export interface FieldState {
  camX: number;
  camY: number;
  camZ: number;
  lookX: number;
  lookY: number;
  lookZ: number;
  /** 0 = untouched signal (brass), 1 = processed by enhancers (violet). */
  process: number;
  /** 0 = whole field, 1 = only the centre row (a single clean stem plot). */
  focus: number;
  /** Samples per wave cycle; higher = higher sample rate. */
  density: number;
  /** Quantisation levels per half-scale; 1 = one bit (DSD). */
  levels: number;
  amp: number;
  /** Overall brightness, used to dim the field behind dense content. */
  visible: number;
}

export const HERO: FieldState = {
  camX: -7.5,
  camY: 2.4,
  camZ: 10.5,
  lookX: 2.5,
  lookY: -1.3,
  lookZ: 0,
  process: 0,
  focus: 0,
  density: 1,
  levels: 7,
  amp: 1,
  visible: 1,
};

const BRASS = new THREE.Color("#c9a45c");
const VIOLET = new THREE.Color("#8c7ae6");
const BG = new THREE.Color("#121d23");

const COMMON = /* glsl */ `
  uniform float uTime;
  uniform float uProcess;
  uniform float uFocus;
  uniform float uDensity;
  uniform float uLevels;
  uniform float uAmp;
  uniform float uWidth;
  uniform float uDepth;

  attribute vec2 aGrid; // x: 0..1 along time, y: 0..1 across rows

  varying float vFade;
  varying float vLevel;

  float signal(float x, float z) {
    float f = 1.0 / uDensity;
    float t = uTime;
    // A "recording": three partials drifting against each other.
    float s = 0.55 * sin(x * 0.55 * f - t * 1.05 + z * 1.7)
            + 0.28 * sin(x * 1.35 * f + t * 0.66 - z * 2.3)
            + 0.17 * sin(x * 2.90 * f - t * 1.80 + z * 0.6);
    // Processed: gentler top end and a slower swell — the sound of an EQ and crossfeed.
    float p = 0.72 * sin(x * 0.42 * f - t * 0.80 + z * 1.2)
            + 0.26 * sin(x * 0.95 * f + t * 0.45 - z * 0.7);
    s = mix(s, p, uProcess * 0.75);
    // Quieter towards the edges of the field so it dissolves rather than ends.
    float edge = smoothstep(0.0, 0.18, aGrid.x) * smoothstep(1.0, 0.82, aGrid.x);
    return s * edge;
  }

  float quantise(float s) {
    // 1 level = 1 bit: only the sign survives, which is what DSD is.
    if (uLevels < 1.5) return s >= 0.0 ? 0.55 : -0.55;
    return floor(s * uLevels + 0.5) / uLevels;
  }

  vec3 place(float end) {
    float x = (aGrid.x - 0.5) * uWidth;
    float zn = aGrid.y * 2.0 - 1.0;
    float z = zn * uDepth * 0.5;
    float centre = exp(-pow(zn * 9.0, 2.0));
    float rowWeight = mix(1.0, centre, uFocus);
    float v = quantise(signal(x, zn)) * uAmp * 1.6;
    vLevel = abs(v);
    vFade = rowWeight;
    return vec3(x, v * end, z);
  }

  float depthFade(vec4 mv) {
    float d = -mv.z;
    return smoothstep(34.0, 9.0, d) * smoothstep(0.6, 2.5, d);
  }
`;

const POINTS_VERT = /* glsl */ `
  ${COMMON}
  uniform float uPixel;
  void main() {
    vec3 p = place(1.0);
    vec4 mv = modelViewMatrix * vec4(p, 1.0);
    vFade *= depthFade(mv);
    gl_Position = projectionMatrix * mv;
    gl_PointSize = (2.2 + vLevel * 1.4) * uPixel * (12.0 / -mv.z);
  }
`;

const POINTS_FRAG = /* glsl */ `
  uniform vec3 uBrass;
  uniform vec3 uViolet;
  uniform float uProcess;
  uniform float uVisible;
  varying float vFade;
  varying float vLevel;
  void main() {
    float d = length(gl_PointCoord - 0.5);
    float a = smoothstep(0.5, 0.0, d);
    a = a * a;
    vec3 c = mix(uBrass, uViolet, uProcess);
    c = mix(c, vec3(1.0, 0.95, 0.86), smoothstep(0.12, 0.0, d) * 0.6);
    gl_FragColor = vec4(c, a * vFade * uVisible * (0.55 + vLevel * 0.35));
  }
`;

const STEMS_VERT = /* glsl */ `
  ${COMMON}
  attribute float aEnd;
  varying float vEnd;
  void main() {
    vec3 p = place(aEnd);
    vEnd = aEnd;
    vec4 mv = modelViewMatrix * vec4(p, 1.0);
    vFade *= depthFade(mv);
    gl_Position = projectionMatrix * mv;
  }
`;

const STEMS_FRAG = /* glsl */ `
  uniform vec3 uBrass;
  uniform vec3 uViolet;
  uniform float uProcess;
  uniform float uVisible;
  varying float vFade;
  varying float vEnd;
  void main() {
    vec3 c = mix(uBrass, uViolet, uProcess);
    // Stems brighten towards their sample head.
    gl_FragColor = vec4(c, vFade * uVisible * (0.08 + vEnd * 0.32));
  }
`;

export interface Field {
  state: FieldState;
  /** Pointer in -1..1, for a slight parallax in the hero. */
  pointer: { x: number; y: number };
  setMotion(enabled: boolean): void;
  resize(): void;
}

/** `mirror`: right-to-left pages put the copy on the right, so the camera swings the other way. */
export function createField(canvas: HTMLCanvasElement, opts: { mirror?: boolean } = {}): Field | null {
  const flip = opts.mirror ? -1 : 1;
  let renderer: THREE.WebGLRenderer;
  try {
    renderer = new THREE.WebGLRenderer({ canvas, antialias: true, powerPreference: "high-performance" });
  } catch {
    return null; // No WebGL: the page reads fine on its own background.
  }
  renderer.setClearColor(BG, 1);

  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(38, 1, 0.1, 100);
  const look = new THREE.Vector3();

  const small = Math.min(window.innerWidth, window.innerHeight) < 700;
  const NX = small ? 150 : 240;
  const NZ = small ? 25 : 41; // odd, so there is an exact centre row
  const width = 30;
  const depth = small ? 12 : 16;

  const grid = new Float32Array(NX * NZ * 2);
  const stemGrid = new Float32Array(NX * NZ * 4);
  const stemEnd = new Float32Array(NX * NZ * 2);
  let i = 0;
  for (let z = 0; z < NZ; z++) {
    for (let x = 0; x < NX; x++) {
      const u = x / (NX - 1);
      const v = z / (NZ - 1);
      grid[i * 2] = u;
      grid[i * 2 + 1] = v;
      stemGrid.set([u, v, u, v], i * 4);
      stemEnd.set([0, 1], i * 2);
      i++;
    }
  }

  const uniforms = {
    uTime: { value: 0 },
    uProcess: { value: 0 },
    uFocus: { value: 0 },
    uDensity: { value: 1 },
    uLevels: { value: 7 },
    uAmp: { value: 1 },
    uVisible: { value: 1 },
    uWidth: { value: width },
    uDepth: { value: depth },
    uPixel: { value: 1 },
    uBrass: { value: BRASS },
    uViolet: { value: VIOLET },
  };

  // Positions are computed in the shader; three still wants a position attribute for bounds.
  const pointsGeo = new THREE.BufferGeometry();
  pointsGeo.setAttribute("position", new THREE.BufferAttribute(new Float32Array(NX * NZ * 3), 3));
  pointsGeo.setAttribute("aGrid", new THREE.BufferAttribute(grid, 2));
  const stemsGeo = new THREE.BufferGeometry();
  stemsGeo.setAttribute("position", new THREE.BufferAttribute(new Float32Array(NX * NZ * 6), 3));
  stemsGeo.setAttribute("aGrid", new THREE.BufferAttribute(stemGrid, 2));
  stemsGeo.setAttribute("aEnd", new THREE.BufferAttribute(stemEnd, 1));

  const shared = { uniforms, transparent: true, depthWrite: false, blending: THREE.AdditiveBlending };
  const points = new THREE.Points(pointsGeo, new THREE.ShaderMaterial({ ...shared, vertexShader: POINTS_VERT, fragmentShader: POINTS_FRAG }));
  const stems = new THREE.LineSegments(stemsGeo, new THREE.ShaderMaterial({ ...shared, vertexShader: STEMS_VERT, fragmentShader: STEMS_FRAG }));
  points.frustumCulled = false;
  stems.frustumCulled = false;
  scene.add(stems, points);

  const state: FieldState = { ...HERO };
  const pointer = { x: 0, y: 0 };
  const eased = { x: 0, y: 0 };
  let motion = true;
  let time = 0;
  let last = performance.now();

  function resize() {
    const w = window.innerWidth;
    const h = window.innerHeight;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    renderer.setPixelRatio(dpr);
    renderer.setSize(w, h, false);
    camera.aspect = w / h;
    // Portrait screens: widen the lens so the field still reads as a landscape.
    camera.fov = w < h ? 52 : 38;
    camera.updateProjectionMatrix();
    uniforms.uPixel.value = dpr * Math.min(1.4, Math.max(0.8, h / 900));
  }

  function frame(now: number) {
    const dt = Math.min(0.05, (now - last) / 1000);
    last = now;
    if (motion) time += dt;

    eased.x += (pointer.x - eased.x) * 0.04;
    eased.y += (pointer.y - eased.y) * 0.04;
    const sway = 1 - state.focus;

    uniforms.uTime.value = time;
    uniforms.uProcess.value = state.process;
    uniforms.uFocus.value = state.focus;
    uniforms.uDensity.value = state.density;
    uniforms.uLevels.value = state.levels;
    uniforms.uAmp.value = state.amp;
    uniforms.uVisible.value = state.visible;

    camera.position.set(flip * state.camX + eased.x * 0.9 * sway, state.camY - eased.y * 0.5 * sway, state.camZ);
    look.set(flip * state.lookX, state.lookY, state.lookZ);
    camera.lookAt(look);
    renderer.render(scene, camera);
  }

  resize();
  window.addEventListener("resize", resize);
  renderer.setAnimationLoop(frame);

  return {
    state,
    pointer,
    resize,
    setMotion(enabled) {
      motion = enabled;
    },
  };
}
