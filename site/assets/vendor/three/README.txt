three.min.js is a minified subset of three.js r186 (npm three@0.186.1, MIT licence, see LICENSE).
It is loaded only by ../../landing3d.js, which adds the optional 3D layer on the landing page.

Exported names:
  WebGLRenderer, Scene, PerspectiveCamera, Group, BufferGeometry, BufferAttribute,
  Points, LineSegments, Mesh, ShaderMaterial, ExtrudeGeometry, ShapeGeometry, Shape,
  EdgesGeometry, PlaneGeometry, IcosahedronGeometry, BoxGeometry, Vector3,
  AdditiveBlending, NormalBlending, DoubleSide, BackSide, FrontSide, DynamicDrawUsage

To rebuild after landing3d.js starts using another THREE name:
  1. npm install three@0.186.1 esbuild
  2. Create subset-entry.js:
       export { <names above> } from "three";
  3. npx esbuild subset-entry.js --bundle --format=esm --minify --legal-comments=eof --outfile=three.min.js
