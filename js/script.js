const smMetadataUrl = "https://storage.googleapis.com/map-rando-videos/";
var frameOffsets;
var animationEnabled = false;
var animationFrameResolution = 3;
var animationFrame = 0;
var roomSummary;

function readSlice(file, start, size) {
    return new Promise(function(resolve, reject) {
        var reader = new FileReader();
        reader.onload = function() { 
            dv = new DataView(reader.result, 0);
            resolve(dv)
        };
        reader.readAsArrayBuffer(file.slice(start, start + size));
    });
}

async function loadAVIMetadata(file) {
    var topDV = await readSlice(file, 0, 24);
    if (topDV.getUint32(0) != 0x52494646) { 
        return Promise.reject("bad header: RIFF");
    }
    if (topDV.getUint32(8) != 0x41564920) {
        return Promise.reject("bad header: AVI");
    }
    if (topDV.getUint32(12) != 0x4C495354) {
        return Promise.reject("bad header: LIST");
    }
    if (topDV.getUint32(20) != 0x6864726C) {
        return Promise.reject("bad header: hdrl");
    }
    
    var headerListSize = topDV.getUint32(16, true) - 4;
    var headerListStart = 24;
    headerDV = await readSlice(file, headerListStart, headerListSize);
    if (headerDV.getUint32(0) != 0x61766968) {
        return Promise.reject("bad header: avih");
    }
    var avihStart = 8;
    avihSize = headerDV.getUint32(4, true);

    // Extract relevant fields from 'avih' header:
    totalFrames = headerDV.getUint32(avihStart + 16, true);
    width = headerDV.getUint32(avihStart + 32, true);
    height = headerDV.getUint32(avihStart + 36, true);
    
    var strlStart = avihStart + avihSize;
    if (headerDV.getUint32(strlStart) != 0x4C495354) {
        return Promise.reject("bad header: strl LIST");
    }
    var strlSize = headerDV.getUint32(strlStart + 4, true);
    if (headerDV.getUint32(strlStart + 8) != 0x7374726C) {
        return Promise.reject("bad header: strl");
    }
    if (headerDV.getUint32(strlStart + 12) != 0x73747268) {
        return Promise.reject("bad header: strh");
    }
    var strhSize = headerDV.getUint32(strlStart + 16, true);
    var strhStart = strlStart + 20;

    // Extract relevant fields from video 'strh' header:
    if (headerDV.getUint32(strhStart, 0) != 0x76696473) {
        return Promise.reject("bad header: vids");
    }
    var rate = headerDV.getUint32(strhStart + 24, true);
    var fps = rate / 1000000;
    
    if (headerDV.getUint32(strhStart + strhSize) != 0x73747266) {
        return Promise.reject("bad header: strf");
    }
    var strfSize = headerDV.getUint32(strhStart + strhSize + 4, true);
    var strfStart = strhStart + strhSize + 8;

    // Extract relevant fields from video 'strf' header:
    var width1 = headerDV.getUint32(strfStart + 4, true);
    var height1 = headerDV.getUint32(strfStart + 8, true);
    var bitcount = headerDV.getUint16(strfStart + 14, true);
    var compression = headerDV.getUint32(strfStart + 16, true);
    if (width1 != width) {
        return Promise.reject(`inconsistent width in strf: ${width1} vs ${width}`);
    }
    if (height1 != height) {
        return Promise.reject(`inconsistent height in strf: ${height1} vs ${height}`);
    }
    if (bitcount != 24) {
        return Promise.reject(`unexpected bitcount (not 24): ${bitcount}`);
    }
    if (compression != 0) {
        return Promise.reject(`unexpected compression: ${compression}`);
    }

    var postHeaderStart = headerListStart + headerListSize;
    var postHeaderDV = await readSlice(file, postHeaderStart, 12);
    var moviStart;
    if (postHeaderDV.getUint32(0) == 0x4A554E4B) {
        junkSize = postHeaderDV.getUint32(4, true);
        moviStart = postHeaderStart + 8 + junkSize;
    } else {
        moviStart = postHeaderStart;
    }

    var moviDV = await readSlice(file, moviStart, 12);
    if (moviDV.getUint32(0) != 0x4C495354) {
        return Promise.reject("bad header: movi LIST");
    }
    if (moviDV.getUint32(8) != 0x6D6F7669) {
        return Promise.reject("bad header: movi");
    }
    var moviSize = moviDV.getUint32(4, true);

    var idxStart = moviStart + 8 + moviSize;
    var idxHeaderDV = await readSlice(file, idxStart, 8);
    if (idxHeaderDV.getUint32(0) != 0x69647831) {
        return Promise.reject("missing index: idx1");
    }
    var idxSize = idxHeaderDV.getUint32(4, true);
    
    var idxDV = await readSlice(file, idxStart + 8, idxSize);
    var pos = 0;
    
    frameOffsets = [];
    while (pos < idxSize) {
        chunkId = idxDV.getUint32(pos, true);
        flags = idxDV.getUint32(pos + 4, true);
        offset = idxDV.getUint32(pos + 8, true);
        size = idxDV.getUint32(pos + 12, true);
        if (chunkId == 0x62643030) {
            frameOffsets.push([file, offset + moviStart + 8]);
        }
        pos += 16;
    }

    if (totalFrames != frameOffsets.length) {
        return Promise.reject(`index video frame count ${frameOffsets.length} does not match total frame count ${totalFrames}`);
    }

    console.log(`Loaded video header: ${width} x ${height}, ${fps} fps, ${totalFrames} frames`);
}

function updateCanvas(canvas, rgbData, size, offsetX, offsetY) {
    canvas.width = size;
    canvas.height = size;

    const ctx = canvas.getContext('2d');
    const imageData = ctx.createImageData(size, size);
    let dstIdx = 0;
    for (let y = 0; y < size; y += 1) {
        for (let x = 0; x < size; x += 1) {
            let srcIdx = ((223 - (offsetY + y)) * 256 + offsetX + x) * 3;
            imageData.data[dstIdx] = rgbData.getUint8(srcIdx + 1);     // R
            imageData.data[dstIdx + 1] = rgbData.getUint8(srcIdx);     // G
            imageData.data[dstIdx + 2] = rgbData.getUint8(srcIdx + 2); // B
            imageData.data[dstIdx + 3] = 255;                          // A (fully opaque)
            dstIdx += 4;
        }
    }
    ctx.putImageData(imageData, 0, 0);
}

async function updateAnimation() {
    var size = parseInt(document.getElementById("thumbnailSize").value);
    var thumbnail = document.getElementById("thumbnail");
    var startFrame = parseInt(document.getElementById("highlightStartTime").value);
    var endFrame = parseInt(document.getElementById("highlightEndTime").value);
    var thumbnailX = document.getElementById("thumbnailX");
    var thumbnailY = document.getElementById("thumbnailY");
    var centerX = parseInt(thumbnailX.value);
    var centerY = parseInt(thumbnailY.value);
    var offsetX = centerX - Math.floor(size / 2);
    var offsetY = centerY - Math.floor(size / 2);

    if (animationFrame > endFrame || animationFrame < startFrame) {
        animationFrame = startFrame;
    }
    var frame = frameOffsets[animationFrame];
    var file = frame[0];
    var byteOffset = frame[1];
    var rgbData = await readSlice(file, byteOffset, 256 * 224 * 3);
    updateCanvas(thumbnail, rgbData, size, offsetX, offsetY);
    animationFrame += animationFrameResolution;
}

async function updatePreview() {
    var size = parseInt(document.getElementById("thumbnailSize").value);
    var thumbnailX = document.getElementById("thumbnailX");
    var thumbnailY = document.getElementById("thumbnailY");
    var centerX = parseInt(thumbnailX.value);
    var centerY = parseInt(thumbnailY.value);
    var offsetX = centerX - Math.floor(size / 2);
    var offsetY = centerY - Math.floor(size / 2);

    if (!animationEnabled) {
        var thumbnail = document.getElementById("thumbnail");
        var thumbnailTime = document.getElementById("thumbnailTime");
        if (thumbnailTime.value != "") {
            var t = parseInt(thumbnailTime.value);
            if (t > frameOffsets.length - 1) {
                thumbnailTime.value = frameOffsets.length - 1;
                t = frameOffsets.length - 1;
            }
            var frame = frameOffsets[t];
            var file = frame[0];
            var byteOffset = frame[1];
            var rgbData = await readSlice(file, byteOffset, 256 * 224 * 3);
            updateCanvas(thumbnail, rgbData, size, offsetX, offsetY);
        }    
    }

    var highlightStart = document.getElementById("highlightStart");
    var highlightStartTime = document.getElementById("highlightStartTime");
    if (highlightStartTime.value != "") {
        var t = parseInt(highlightStartTime.value);
        if (t > frameOffsets.length - 1) {
            highlightStartTime.value = frameOffsets.length - 1;
            t = frameOffsets.length - 1;
        }
        var frame = frameOffsets[t];
        var file = frame[0];
        var byteOffset = frame[1];
        var rgbData = await readSlice(file, byteOffset, 256 * 224 * 3);
        updateCanvas(highlightStart, rgbData, size, offsetX, offsetY);
    }

    var highlightEnd = document.getElementById("highlightEnd");
    var highlightEndTime = document.getElementById("highlightEndTime");
    if (highlightEndTime.value != "") {
        var t = parseInt(highlightEndTime.value);
        if (t > frameOffsets.length - 1) {
            highlightEndTime.value = frameOffsets.length - 1;
            t = frameOffsets.length - 1;
        }
        var frame = frameOffsets[t];
        var file = frame[0];
        var byteOffset = frame[1];
        var rgbData = await readSlice(file, byteOffset, 256 * 224 * 3);
        updateCanvas(highlightEnd, rgbData, size, offsetX, offsetY);
    }
}

async function updateControls() {
    var size = parseInt(document.getElementById("thumbnailSize").value);
    var thumbnailX = document.getElementById("thumbnailX");
    var thumbnailY = document.getElementById("thumbnailY");
    var centerX = parseInt(thumbnailX.value);
    var centerY = parseInt(thumbnailY.value);

    minThumbnailX = Math.floor(size / 2);
    maxThumbnailX = 256 - Math.floor(size / 2);
    minThumbnailY = Math.floor(size / 2);
    maxThumbnailY = 224 - Math.floor(size / 2);
    thumbnailX.min = minThumbnailX;
    thumbnailX.max = maxThumbnailX;
    thumbnailY.min = minThumbnailY;
    thumbnailY.max = maxThumbnailY;
    if (centerX < minThumbnailX) {
        console.log(`min before: ${centerX}`);
        centerX = minThumbnailX;
        thumbnailX.value = centerX;
        console.log(`min after: ${centerX}`);
    }
    if (centerX > maxThumbnailX) {
        console.log(`max before: ${centerX}, max=${thumbnailX.max}`);
        centerX = maxThumbnailX;
        thumbnailX.value = centerX;
        console.log(`max after: ${centerX}`);
    }
    if (centerY < minThumbnailY) {
        centerY = minThumbnailY;
        thumbnailY.value = centerY;
    }
    if (centerY > maxThumbnailY) {
        centerY = maxThumbnailY;
        thumbnailY.value = centerY;
    }
    updatePreview();
}

async function updateFile() {
    var videoFile = document.getElementById("videoFile");
    // TODO: handle multiple files
    var file = videoFile.files[0];
    await loadAVIMetadata(file);

    var thumbnailTime = document.getElementById("thumbnailTime")
    thumbnailTime.value = 300;
    thumbnailTime.max = totalFrames - 1;

    var highlightStartTime = document.getElementById("highlightStartTime")
    highlightStartTime.value = 180;
    highlightStartTime.max = totalFrames - 1;

    var highlightEndTime = document.getElementById("highlightEndTime")
    highlightEndTime.value = 420;
    highlightEndTime.max = totalFrames - 1;

    updateControls();
}

async function updateRoomOptions() {
    let overviewResponse = await fetch(smMetadataUrl + "rooms.json");
    if (!overviewResponse.ok) {
        throw new Error(`Error fetching rooms.json: ${overviewResponse.status}`);
    }
    let overview = await overviewResponse.json();
    let roomSelect = document.getElementById("room");
    for (const areaData of overview.areas) {
        let optGroup = document.createElement('optgroup');
        optGroup.label = areaData.name;
        for (const roomData of areaData.rooms) {
            var opt = document.createElement('option');
            opt.value = roomData.id;
            opt.innerText = roomData.name;
            optGroup.appendChild(opt);    
        }
        roomSelect.appendChild(optGroup);
    }
}

async function updateNodeOptions() {
    let roomId = document.getElementById("room").value;
    let fromNode = document.getElementById("fromNode");
    let toNode = document.getElementById("toNode");
    let strat = document.getElementById("strat");
    
    fromNode.options.length = 1;
    toNode.options.length = 1;
    strat.options.length = 1;

    if (roomId != "") {
        let roomResponse = await fetch(smMetadataUrl + `room/${roomId}.json`);
        if (!roomResponse.ok) {
            throw new Error(`Error fetching room/${roomId}.json: ${roomResponse.status}`);
        }
        roomSummary = await roomResponse.json();
        for (const node of roomSummary.nodes) {
            var opt = document.createElement('option');
            opt.value = node.id;
            opt.innerText = `${node.id}: ${node.name}`;
            fromNode.appendChild(opt);

            var opt = document.createElement('option');
            opt.value = node.id;
            opt.innerText = `${node.id}: ${node.name}`;
            toNode.appendChild(opt);
        }
    }
}

async function updateStratOptions() {
    let stratSelect = document.getElementById("strat");
    let fromNodeId = document.getElementById("fromNode").value;
    let toNodeId = document.getElementById("toNode").value;

    stratSelect.options.length = 1;
    for (const strat of roomSummary.strats) {
        if (strat.from_node_id == fromNodeId && strat.to_node_id == toNodeId) {
            var opt = document.createElement('option');
            opt.value = strat.id;
            opt.innerText = `${strat.id}: ${strat.name}`;
            stratSelect.appendChild(opt);    
        }
    }
}

function enableAnimation() {
    animationEnabled = true;
}

function disableAnimation() {
    animationEnabled = false;
    updatePreview();
}

async function animateLoop() {
    while (true) {
        await new Promise(r => setTimeout(r, 1000 / 60 * animationFrameResolution));
        if (animationEnabled) {
            updateAnimation();
        }
    }
    console.log("animate");
}

updateRoomOptions();
updateFile();
animateLoop();