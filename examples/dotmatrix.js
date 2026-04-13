const GRID_WIDTH = 1000;
const GRID_HEIGHT = 1000;
const DOT_MARGIN = 0.5;
const MIN_DOT_SIZE = 8;
const MAX_DOT_SIZE = 128;
let dotSize = 16;
let colOffset = 0;
let rowOffset = 0;

let bitmap = undefined;

const canvas = document.getElementById("canvas");

function resizeCanvas() {
  canvas.width = window.innerWidth;
  canvas.height = window.innerHeight;
  draw();
}

resizeCanvas();

window.addEventListener("resize", resizeCanvas);

canvas.addEventListener("click", (ev) => {
  let col = Math.floor(ev.offsetX / (dotSize + DOT_MARGIN * 2));
  let row = Math.floor(ev.offsetY / (dotSize + DOT_MARGIN * 2));

  let bitIdx = GRID_WIDTH * (row + rowOffset) + col + colOffset;

  let currentState = bitmap !== undefined && ((bitmap[Math.floor(bitIdx / 8)] & (1 << (bitIdx % 8))) !== 0);
  setBit(bitIdx, !currentState, true);
});

canvas.addEventListener("wheel", (ev) => {
  if (ev.deltaY < 0 && (dotSize < MAX_DOT_SIZE)) {
    dotSize += 1;
  } else if(dotSize > MIN_DOT_SIZE) {
    dotSize -= 1;
  }
  draw();
});


canvas.addEventListener("pointerdown", (ev) => {
  console.log(ev);
});
canvas.addEventListener("pointerup", (ev) => {
  console.log(ev);
});
canvas.addEventListener("pointermove", (ev) => {
  console.log(ev);
});

ws = new WebSocket("ws://" + window.location.host);

ws.onopen = (_event) => {
  draw();

  ws.onmessage = (event) => {
    event.data.arrayBuffer()
      .then(buffer => {
        bitmap = new Uint8Array(buffer);
        draw();

        ws.onmessage = (event) => {
          event.data.arrayBuffer()
            .then(buffer => {
              let byte = new Uint32Array(buffer)[1]
              let idx = byte & 0x7FFFFFFF;
              let status = (byte >> 31 & 1) === 1;
              setBit(idx, status, false);
            })
            .catch(e => { console.log(e) })
        };
      })
      .catch(e => { console.log(e) })
  };
}

function setBit(idx, status, send) {
  if (status) {
    bitmap[Math.floor(idx / 8)] |= (1 << (idx % 8));
    draw();
    if (send) {
      ws.send(new Uint32Array([0x1, 0x80000000 + idx]));
    }
  } else {
    bitmap[Math.floor(idx / 8)] &= ~(1 << (idx % 8));
    draw();
    if (send) {
      ws.send(new Uint32Array([0x1, idx]));
    }
  }
}

function draw() {
  const ctx = canvas.getContext("2d");

  let cols = Math.min(Math.ceil(canvas.width / (dotSize + DOT_MARGIN * 2)), GRID_WIDTH);
  let rows = Math.min(Math.ceil(canvas.height / (dotSize + DOT_MARGIN * 2)), GRID_HEIGHT);

  ctx.fillStyle = "#171717";
  ctx.fillRect(0, 0, canvas.width, canvas.height);

  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      let bitIdx = GRID_WIDTH * (row + rowOffset) + col + colOffset;
      let x = (dotSize + DOT_MARGIN * 2) * col;
      let y = (dotSize + DOT_MARGIN * 2) * row;

      if (bitmap !== undefined && ((bitmap[Math.floor(bitIdx / 8)] & (1 << (bitIdx % 8))) !== 0)) {
        ctx.fillStyle = "#f57200";
      } else {
        ctx.fillStyle = "#1a1a1a";
      }
      ctx.fillRect(x, y, dotSize, dotSize);
    }
  }
}
