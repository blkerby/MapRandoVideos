// TODO: Split this up to make it more manageable.
// It would probably be better to separate the HTML-serving API form the backend endpoints.
// Maybe use some kind of modern framework and make it less of a mess?
var frameOffsets = null;
var animationEnabled = false;
var animationDomPrefix = null;
var animationFrameResolution = 3;
var animationFrame = 0;
var videoId = null;
var videoList = [];
var videoLimitIncrement = 10;
var videoLimit = videoLimitIncrement;
var numVideoParts = null;
var startUploadKey = null;
var finishUploadKey = null;
var submitting = false;
var userMapping = null;
var controlsUpdated = false;
var updatedTech = new Set();
var updatedNotables = new Set();

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

async function loadAVIMetadata(file, localFrameOffsets) {
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
    let headerDV = await readSlice(file, headerListStart, headerListSize);
    if (headerDV.getUint32(0) != 0x61766968) {
        return Promise.reject("bad header: avih");
    }
    var avihStart = 8;
    let avihSize = headerDV.getUint32(4, true);

    // Extract relevant fields from 'avih' header:
    let totalFrames = headerDV.getUint32(avihStart + 16, true);
    let width = headerDV.getUint32(avihStart + 32, true);
    let height = headerDV.getUint32(avihStart + 36, true);
    
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

    var moviStart = headerListStart + headerListSize;
    var moviDV = await readSlice(file, moviStart, 12);
    
    while (moviDV.getUint32(0) != 0x4C495354 || moviDV.getUint32(8) != 0x6D6F7669) {
        // Skip over irrelevant chunks before we get to the "movi" chunk
        junkSize = moviDV.getUint32(4, true);
        moviStart += junkSize + 8;
        moviDV = await readSlice(file, moviStart, 12);
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
    var cnt = 0;
    while (pos < idxSize) {
        chunkId = idxDV.getUint32(pos, true);
        flags = idxDV.getUint32(pos + 4, true);
        offset = idxDV.getUint32(pos + 8, true);
        size = idxDV.getUint32(pos + 12, true);
        if ((chunkId == 0x62643030 || chunkId == 0x63643030) && size != 0) {
            localFrameOffsets.push([file, offset + moviStart + 17]);
            cnt += 1;
        }
        pos += 16;
    }

    if (totalFrames != cnt) {
        // We allow some lenience for what Bizhawk does with multipart AVIs, where there may be 1 audio frame with no video?
        // A difference of more than one frame indicates a bigger problem
        console.log(`index video frame count ${cnt} does not match total frame count ${totalFrames}`);

        if (Math.abs(totalFrames - cnt) > 1) {
            return Promise.reject("wrong frame count");
        }
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

async function updateAnimation(domIdPrefix) {
    if (frameOffsets == null) {
        return;
    }
    var size = parseInt(document.getElementById(domIdPrefix + "cropSize").value);
    var thumbnail = document.getElementById(domIdPrefix + "thumbnail");
    var startFrame = parseInt(document.getElementById(domIdPrefix + "highlightStartTime").value);
    var endFrame = parseInt(document.getElementById(domIdPrefix + "highlightEndTime").value);
    var thumbnailX = document.getElementById(domIdPrefix + "cropCenterX");
    var thumbnailY = document.getElementById(domIdPrefix + "cropCenterY");
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

async function updatePreview(domIdPrefix) {
    if (frameOffsets == null) {
        return;
    }
    var size = parseInt(document.getElementById(domIdPrefix + "cropSize").value);
    var thumbnailX = document.getElementById(domIdPrefix + "cropCenterX");
    var thumbnailY = document.getElementById(domIdPrefix + "cropCenterY");
    var centerX = parseInt(thumbnailX.value);
    var centerY = parseInt(thumbnailY.value);
    var offsetX = centerX - Math.floor(size / 2);
    var offsetY = centerY - Math.floor(size / 2);

    if (!animationEnabled) {
        var thumbnail = document.getElementById(domIdPrefix + "thumbnail");
        var thumbnailTime = document.getElementById(domIdPrefix + "thumbnailTime");
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

    var highlightStart = document.getElementById(domIdPrefix + "highlightStart");
    var highlightStartTime = document.getElementById(domIdPrefix + "highlightStartTime");
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

    var highlightEnd = document.getElementById(domIdPrefix + "highlightEnd");
    var highlightEndTime = document.getElementById(domIdPrefix + "highlightEndTime");
    highlightEndTime.min = parseInt(highlightStartTime.value) + 1;
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

async function updateControls(domIdPrefix) {
    controlsUpdated = true;
    var size = parseInt(document.getElementById(domIdPrefix + "cropSize").value);
    var thumbnailX = document.getElementById(domIdPrefix + "cropCenterX");
    var thumbnailY = document.getElementById(domIdPrefix + "cropCenterY");
    var centerX = parseInt(thumbnailX.value);
    var centerY = parseInt(thumbnailY.value);

    minThumbnailX = Math.floor(size / 2);
    maxThumbnailX = 256 - Math.floor(size / 2);
    minThumbnailY = Math.floor(size / 2);
    maxThumbnailY = 224 - Math.ceil(size / 2);
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
    if (frameOffsets == null) {
        return;
    }
    updatePreview(domIdPrefix);
}

async function readFileChunks(file, chunkSize) {
    var pos = 0;
    var chunks = [];
    while (true) {
        const slice = file.slice(pos, pos + chunkSize);
        const arrayBuf = await slice.arrayBuffer(slice);
        const data = new Uint8Array(arrayBuf);
        if (data.length == 0) {
            return chunks;
        }
        chunks.push(data);
        pos += chunkSize;
    }
}

async function updateFile() {
    videoId = null;
    let uploadKey = Math.random();
    startUploadKey = uploadKey;
    var videoFile = document.getElementById("videoFile");
    if (videoFile.files.length == 0) {
        return;
    }

    // Sort the files alphabetically by filename:
    var fileList = [];
    for (var file of videoFile.files) {
        fileList.push(file);
    }
    fileList.sort((a, b) => {
        if (a.name < b.name) return -1;
        if (a.name > b.name) return 1;
        return 0;
    });

    var localFrameOffsets = [];
    for (const file of fileList) {
        await loadAVIMetadata(file, localFrameOffsets);
    }

    document.getElementById("thumbnail").classList.remove("d-none");
    document.getElementById("highlightStart").classList.remove("d-none");
    document.getElementById("highlightEnd").classList.remove("d-none");

    document.getElementById("cropSize").value = 128;
    document.getElementById("cropCenterX").value = 128;
    document.getElementById("cropCenterY").value = 128;

    frameOffsets = localFrameOffsets;

    var thumbnailTime = document.getElementById("thumbnailTime")
    thumbnailTime.value = 300;
    thumbnailTime.max = frameOffsets.length - 1;

    var highlightStartTime = document.getElementById("highlightStartTime")
    highlightStartTime.value = 180;
    highlightStartTime.max = frameOffsets.length - 1;

    var highlightEndTime = document.getElementById("highlightEndTime")
    highlightEndTime.value = 420;
    highlightEndTime.max = frameOffsets.length - 1;

    updateControls('');
    var newVideoId = null;
    let username = localStorage.getItem("username");
    let token = localStorage.getItem("token");
    
    for (var i = 0; i < fileList.length; i++) {
        var start = performance.now();
        var file = fileList[i];
        var compressedStream = file.stream().pipeThrough(new CompressionStream("gzip"));
        var compressedData = new Uint8Array(await new Response(compressedStream).arrayBuffer());
        var elapsedTime = performance.now() - start;
        console.log(`part ${i}: compression time elapsed (ms)=${elapsedTime}, compressed size=${compressedData.length}`);
    
        var start = performance.now();
        var headers = {
            "Content-Type": "video/avi",
            "Content-Encoding": "gzip",
            "Authorization": 'Basic ' + btoa(username + ":" + token),
            "X-MapRandoVideos-NumParts": videoFile.files.length,
            "X-MapRandoVideos-PartNum": i,
        };
        if (newVideoId !== null) {
            headers["X-MapRandoVideos-VideoId"] = newVideoId;
        }
        var uploadResponse = await fetch("/upload-video", {
            method: "POST",
            headers: headers,
            body: compressedData,
        });
        var elapsedTime = performance.now() - start;
        console.log(`part ${i}: upload time elapsed (ms)=${elapsedTime}`);
    
        if (!uploadResponse.ok) {
            throw new Error(`Error uploading video: ${uploadResponse.status}`);
        }
        newVideoId = parseInt(await uploadResponse.text());
        videoId = newVideoId;
    }
    finishUploadKey = uploadKey;
    console.log("finished uploading video: id=" + newVideoId);
}

async function updateRoomOptions(roomSelectList) {
    let overviewResponse = await fetch("/rooms-by-area");
    if (!overviewResponse.ok) {
        throw new Error(`Error fetching rooms.json: ${overviewResponse.status}`);
    }
    let overview = await overviewResponse.json();
    for (const roomSelect of roomSelectList) {
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
}

async function updateNodeOptions(roomDomId, fromDomId, toDomId, stratDomId) {
    let roomId = document.getElementById(roomDomId).value;
    let fromNode = document.getElementById(fromDomId);
    let toNode = document.getElementById(toDomId);
    let strat = document.getElementById(stratDomId);
    
    fromNode.options.length = 1;
    toNode.options.length = 1;
    strat.options.length = 1;

    if (roomId != "") {
        let req = {room_id: roomId};
        let params = new URLSearchParams(req).toString();
        let roomResponse = await fetch(`/nodes?${params}`);
    
        if (!roomResponse.ok) {
            throw new Error(`Error ${roomResponse.status} fetching nodes: ${await roomResponse.text()}`);
        }
        let nodeList = await roomResponse.json();
        for (const node of nodeList) {
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

async function updateStratOptions(roomDomId, stratDomId, fromDomId, toDomId) {
    let roomId = document.getElementById(roomDomId).value;
    let stratSelect = document.getElementById(stratDomId);
    let fromNodeId = document.getElementById(fromDomId).value;
    let toNodeId = document.getElementById(toDomId).value;

    stratSelect.options.length = 1;
    if (roomId == "" || fromNodeId == "" || toNodeId == "") {
        return;
    }
    let req = {room_id: roomId, from_node_id: fromNodeId, to_node_id: toNodeId};
    let params = new URLSearchParams(req).toString();
    let response = await fetch(`/strats?${params}`);

    if (!response.ok) {
        throw new Error(`Error ${response.status} fetching strats: ${await response.text()}`);
    }
    let stratList = await response.json();
    for (const strat of stratList) {
        var opt = document.createElement('option');
        opt.value = strat.id;
        opt.innerText = `${strat.id}: ${strat.name}`;
        stratSelect.appendChild(opt);    
    }
}

function enableAnimation(domIdPrefix) {
    animationEnabled = true;
    animationDomPrefix = domIdPrefix;
}

function disableAnimation(domIdPrefix) {
    animationEnabled = false;
    updatePreview(domIdPrefix);
}

async function animateLoop() {
    while (true) {
        await new Promise(r => setTimeout(r, 1000 / 60 * animationFrameResolution));
        if (animationEnabled) {
            updateAnimation(animationDomPrefix);
        }
    }
}

function updateLogin() {
    let username = localStorage.getItem("username");
    let logoutButton = document.getElementById("logoutButton");
    let loginButton = document.getElementById("loginButton");
    let uploadButton = document.getElementById("uploadButton");
    let techButton = document.getElementById("techButton");
    if (username !== null) {
        logoutButton.innerText = `Log Out (${username})`;
        logoutButton.classList.remove("d-none");
        loginButton.classList.add("d-none");
        uploadButton.classList.remove("d-none");
    } else {
        logoutButton.classList.add("d-none");
        loginButton.classList.remove("d-none");
        uploadButton.classList.add("d-none");
    }
    let permission = localStorage.getItem("permission");
    if (permission == "Editor") {
        techButton.classList.remove("d-none");
    } else {
        techButton.classList.add("d-none");
    }
}

async function signIn() {
    let username = document.getElementById("username").value;
    let token = document.getElementById("token").value;
    
    let response = await fetch("/sign-in", {
      headers: {
        "Authorization": 'Basic ' + btoa(username + ":" + token),
      }  
    });
    if (response.ok) {
        info = await response.json();
        localStorage.setItem("username", username);
        localStorage.setItem("token", token);
        localStorage.setItem("userId", info.user_id);
        localStorage.setItem("permission", info.permission);
        updateLogin();
        bootstrap.Modal.getInstance(document.getElementById("loginModal")).hide();
    } else {
        document.getElementById("loginFailed").classList.remove("d-none");
    }
    updateFilter();
}

function signOut() {
    localStorage.removeItem("username");
    localStorage.removeItem("token");
    localStorage.removeItem("userId");
    localStorage.removeItem("permission");
    updateLogin();
    updateFilter();
}

function tryParseInt(s) {
    if (s == "") {
        return null;
    } else {
        return parseInt(s);
    }
}

async function submitVideo() {
    if (submitting) {
        return;
    }
    var form = document.getElementById("uploadForm");
    if (!form.checkValidity()) {
        console.log("invalid form");
        form.classList.add('was-validated');
        return;
    }
    submitting = true;
    let submitModal = new bootstrap.Modal(document.getElementById("submitModal"));
    let uploadModal = bootstrap.Modal.getInstance(document.getElementById("uploadModal"));
    submitModal.show();
    uploadModal.hide();
    while (startUploadKey === null || startUploadKey != finishUploadKey) {
        // Sleep for 200 ms
        await new Promise(r => setTimeout(r, 200));
    }
    var formData = new FormData(form);
    let req = {
        video_id: videoId,
        room_id: tryParseInt(formData.get("room_id")),
        from_node_id: tryParseInt(formData.get("from_node_id")),
        to_node_id: tryParseInt(formData.get("to_node_id")),
        strat_id: tryParseInt(formData.get("strat_id")),
        note: formData.get("note"),
        dev_note: formData.get("dev_note"),
        crop_size: tryParseInt(formData.get("crop_size")),
        crop_center_x: tryParseInt(formData.get("crop_center_x")),
        crop_center_y: tryParseInt(formData.get("crop_center_y")),
        thumbnail_t: tryParseInt(formData.get("thumbnail_t")),
        highlight_start_t: tryParseInt(formData.get("highlight_start_t")),
        highlight_end_t: tryParseInt(formData.get("highlight_end_t")),
        copyright_waiver: formData.get("copyright_waiver") == "on",
    };
    var json = JSON.stringify(req);

    let username = localStorage.getItem("username");
    let token = localStorage.getItem("token");

    var result = await fetch("/submit-video", {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
            "Authorization": 'Basic ' + btoa(username + ":" + token),
        },
        body: json
    });
    submitting = false;
    submitModal.hide();
    if (result.ok) {
        console.log("Successfully submitted video");
        form.classList.remove("was-validated");
        frameOffsets = null;
        document.getElementById("videoFile").value = null;
        document.getElementById("thumbnail").classList.add("d-none");
        document.getElementById("highlightStart").classList.add("d-none");
        document.getElementById("highlightEnd").classList.add("d-none");
        updateFilter();
    } else {
        resultText = await result.text();
        console.log(`Failed to submit video: ${resultText}`);
        uploadModal.show();
    }
}

var loginModal = document.getElementById('loginModal')
loginModal.addEventListener('show.bs.modal', function (event) {
    document.getElementById("loginFailed").classList.add("d-none");
});

async function updateUserList() {
    let response = await fetch("/list-users");
    if (!response.ok) {
        console.log("Error fetching user list: " + await response.text());
        return;
    }
    let userList = await response.json();
    let userSelect = document.getElementById("filterUser");
    userMapping = {};
    userSelect.options.length = 1;
    for (const userInfo of userList) {
        var opt = document.createElement('option');
        opt.value = userInfo.id;
        opt.innerText = userInfo.username;
        userSelect.appendChild(opt);

        userMapping[userInfo.id] = userInfo.username;
    }
}

function loadVideo(video, userId, permission, dateFormat, videoTableBody) {
    let tr = document.createElement('tr');
    tr.classList.add("video-row");
    let td = document.createElement('td');
    td.classList.add("p-2");
    let row = document.createElement('div');
    row.classList.add("row");

    let imgCol = document.createElement('div');
    imgCol.classList.add("text-center");
    imgCol.classList.add("col-sm-4");
    imgCol.classList.add("col-md-3");
    imgCol.classList.add("col-lg-2");

    let imgA = document.createElement('a');
    imgA.href = "#";
    let videoUrl = videoStorageClientUrl + "/mp4/" + video.id + ".mp4";
    imgA.setAttribute("onclick", `startVideo('${videoUrl}');`);
    imgA.setAttribute("data-bs-toggle", "modal");
    imgA.setAttribute("data-bs-target", "#videoModal");
    imgCol.appendChild(imgA);

    let pngEl = document.createElement('img');
    pngEl.classList.add("png");
    pngEl.loading = "lazy";
    pngEl.src = videoStorageClientUrl + "/png/" + video.id + ".png";
    pngEl.style = "width:128px;";
    imgA.appendChild(pngEl);

    let webpEl = document.createElement('img');
    webpEl.classList.add("webp");
    webpEl.loading = "lazy";
    webpEl.src = videoStorageClientUrl + "/png/" + video.id + ".png";
    webpEl.fetchPriority = "low";
    webpEl.style = "width:128px;";
    imgA.appendChild(webpEl);
    tr.addEventListener("mouseenter", function() {
        webpEl.src = videoStorageClientUrl + "/webp/" + video.id + ".webp";
    });

    let textCol = document.createElement('div');
    textCol.classList.add("col-sm-8");
    textCol.classList.add("col-md-7");
    textCol.classList.add("col-lg-8");

    let pSubmitted = document.createElement('p');
    pSubmitted.classList.add("m-0");
    let createdUsername = userMapping[video.created_user_id];
    let submittedTime = new Date();
    submittedTime.setTime(video.submitted_ts);
    let submittedTimeStr = dateFormat.format(submittedTime);
    pSubmitted.innerHTML = `Submitted by <i>${createdUsername}</i> on ${submittedTimeStr}`;
    textCol.appendChild(pSubmitted);

    if (video.updated_ts != video.submitted_ts || video.updated_user_id != video.created_user_id) {
        let pUpdated = document.createElement('p');
        pUpdated.classList.add("m-0");
        let updatedUsername = userMapping[video.updated_user_id];
        let updatedTime = new Date();
        updatedTime.setTime(video.updated_ts);
        let updatedTimeStr = dateFormat.format(updatedTime);
        pUpdated.innerHTML = `Updated by <i>${updatedUsername}</i> on ${updatedTimeStr}`;
        textCol.appendChild(pUpdated);    
    }

    if (video.room_id !== null) {
        let pRoom = document.createElement('p');
        pRoom.classList.add("m-0");
        pRoom.innerText = `Room: ${video.room_name}`;
        textCol.appendChild(pRoom);    
    }

    if (video.from_node_id !== null) {
        let pFromNode = document.createElement('p');
        pFromNode.classList.add("m-0");
        pFromNode.innerText = `From ${video.from_node_id}: ${video.from_node_name}`;
        textCol.appendChild(pFromNode);    
    }

    if (video.to_node_id !== null) {
        let pToNode = document.createElement('p');
        pToNode.classList.add("m-0");
        pToNode.innerText = `To ${video.to_node_id}: ${video.to_node_name}`;
        textCol.appendChild(pToNode);
    }

    if (video.strat_id !== null) {
        let pStrat = document.createElement('p');
        pStrat.classList.add("m-0");
        pStrat.innerText = `Strat ${video.strat_id}: ${video.strat_name}`;
        textCol.appendChild(pStrat);    
    }

    if (video.priority !== null) {
        let pPriority = document.createElement('p');
        pPriority.classList.add("m-0");
        pPriority.innerText = `Priority: ${video.priority}`;
        textCol.appendChild(pPriority);
    }

    if (video.note !== "") {
        let pNote = document.createElement('p');
        pNote.classList.add("m-0");
        pNote.innerText = `Note: ${video.note}`;
        textCol.appendChild(pNote);    
    }

    if (video.dev_note !== "") {
        let pDevNote = document.createElement('p');
        pDevNote.classList.add("m-0");
        pDevNote.innerText = `Dev Note: ${video.dev_note}`;
        textCol.appendChild(pDevNote);    
    }

    let shareCol = document.createElement('div');
    shareCol.classList.add("col-md-2");
    shareCol.classList.add("text-end");

    let videoIdText = document.createElement('div');
    videoIdText.innerText = `Id: ${video.id}`;
    shareCol.appendChild(videoIdText);

    let shareButton = document.createElement('button');
    shareButton.classList.add("btn");
    shareButton.classList.add("btn-secondary");
    shareButton.classList.add("my-1");
    shareButton.classList.add("ms-2");
    shareButton.setAttribute("onclick", `shareVideoLink(this, ${video.id})`);
    shareButton.innerHTML = '<i class="bi bi-clipboard"></i> Share';
    shareCol.appendChild(shareButton);

    if (permission == "Editor" || (userId == video.created_user_id && video.status != "Approved")) {
        let editButton = document.createElement('button');
        editButton.classList.add("btn");
        editButton.classList.add("btn-success");
        editButton.classList.add("my-1");
        editButton.classList.add("ms-2");
        editButton.setAttribute("onclick", `openEditVideo(${video.id})`);
        // editButton.setAttribute("data-bs-toggle", "modal");
        // editButton.setAttribute("data-bs-target", "#editModal");
        editButton.innerHTML = '<i class="bi bi-pencil"></i> Edit';
        shareCol.appendChild(editButton);    
    }

    let pStatus = document.createElement('p');
    pStatus.classList.add("m-0");
    pStatus.innerText = `Status: ${video.status}`;
    shareCol.appendChild(pStatus);

    row.appendChild(imgCol);
    row.appendChild(textCol);
    row.appendChild(shareCol);
    td.appendChild(row);
    tr.appendChild(td);
    videoTableBody.appendChild(tr);
}

async function loadVideoBatch(oldVideoLimit, newVideoLimit) {
    let userId = localStorage.getItem("userId");
    let permission = localStorage.getItem("permission");
    let videoTableBody = document.getElementById("videoTableBody");
    let dateFormat = new Intl.DateTimeFormat(undefined, {
        year: 'numeric',
        month: 'short',
        day: 'numeric',
        hour12: false,
        hour: 'numeric',
        minute: '2-digit',
    });

    for (var i = oldVideoLimit; i < newVideoLimit; i++) {
        const video = videoList[i];
        loadVideo(video, userId, permission, dateFormat, videoTableBody);
    }
    videoLimit = newVideoLimit;
}

async function updateFilter() {
    if (userMapping === null) {
        await updateUserList();
    }

    let room = document.getElementById("filterRoom").value;
    let fromNode = document.getElementById("filterFromNode").value;
    let toNode = document.getElementById("filterToNode").value;
    let strat = document.getElementById("filterStrat").value;
    let user = document.getElementById("filterUser").value;
    let status = document.getElementById("filterStatus").value;
    let notes = document.getElementById("filterNotes").value;
    let statuses = [];
    
    if (status == "") {
        statuses = ["Approved", "Complete", "Incomplete", "Disabled"];
    } else {
        statuses = [status];
    }

    let req = {};
    if (room != "") {
        req.room_id = parseInt(room);
        filterVideoId = null;
    }
    if (fromNode != "") {
        req.from_node_id = parseInt(fromNode);
    }
    if (toNode != "") {
        req.to_node_id = parseInt(toNode);
    }
    if (strat != "") {
        req.strat_id = parseInt(strat);
    }
    if (user != "") {
        req.user_id = parseInt(user);
        filterVideoId = null;
    }
    if (filterVideoId !== null) {
        req.video_id = filterVideoId;
    }
    if (notes !== "") {
        req.notes = notes;
    }
    req.status_list = statuses;
    req.sort_by = document.getElementById("filterSortBy").value;
    
    // The backend supports pagination but we're not using it yet.
    // If we add a lot of videos, consider dynamically loading the table rows as the user scrolls down.
    // For now, we dynamically populate the DOM as the user scrolls, but the JSON is still all loaded up-front.
    req.limit = 10000;

    frameOffsets = null;

    let params = new URLSearchParams(req).toString();
    let result = await fetch(`/list-videos?${params}`);
    if (!result.ok) {
        throw new Error(`HTTP ${result.status} fetching video list: ${await result.text()}`);
    }

    videoList = await result.json();
    document.getElementById("videoCount").innerText = videoList.length;

    let videoTableBody = document.getElementById("videoTableBody");
    videoTableBody.innerHTML = "";
    videoLimit = Math.min(Math.max(videoLimit, videoLimitIncrement), videoList.length);
    loadVideoBatch(0, videoLimit);
}

async function downloadVideos() {
    // Download the video parts into OPFS as /0.avi, /1.avi, etc.
    console.log("storage:" + (await navigator.storage.estimate()).quota);
    let username = localStorage.getItem("username");
    let token = localStorage.getItem("token");
    let dir = await navigator.storage.getDirectory();
    var fileList = [];
    for (var i = 0; i < numVideoParts; i++) {
        var start = performance.now();
        let response = await fetch(
            `/download-video?video_id=${videoId}&part_num=${i}`, {
                headers: {
                    "Authorization": 'Basic ' + btoa(username + ":" + token),
                }
            });
        const ds = new DecompressionStream("gzip");
        const decompressedStream = response.body.pipeThrough(ds);

        const filename = `${i}.avi`;
        const file = await dir.getFileHandle(filename, {create: true});
        const writableStream = await file.createWritable();
        await decompressedStream.pipeTo(writableStream);
        fileList.push(await file.getFile());
        var elapsedTime = performance.now() - start;
        console.log(`finished download part ${i}: elapsed time (ms)=${elapsedTime}`);
    }

    // Parse the AVI metadata:
    let localFrameOffsets = [];
    for (var i = 0; i < numVideoParts; i++) {
        await loadAVIMetadata(fileList[i], localFrameOffsets);
    }
    frameOffsets = localFrameOffsets;
}

async function editShowPreview() {
    console.log(`${videoId}: ${numVideoParts}`);
    await downloadVideos();

    var thumbnailTime = document.getElementById("edit-thumbnailTime")
    thumbnailTime.max = frameOffsets.length - 1;
    console.log("max: " + thumbnailTime.max);

    var highlightStartTime = document.getElementById("edit-highlightStartTime")
    highlightStartTime.max = frameOffsets.length - 1;

    var highlightEndTime = document.getElementById("edit-highlightEndTime")
    highlightEndTime.min = parseInt(highlightStartTime.value) + 1;
    highlightEndTime.max = frameOffsets.length - 1;

    document.getElementById("edit-show-preview").classList.add("d-none");
    document.getElementById("edit-preview").classList.remove("d-none");
    updatePreview('edit-');
    bootstrap.Modal.getInstance(document.getElementById("downloadingModal")).hide();
    bootstrap.Modal.getInstance(document.getElementById("editModal")).show();
}

function shareVideoLink(el, id) {
    let oldHTML = el.innerHTML;
    el.innerHTML = '<i class="bi bi-check2"></i> Copied';
    let link = window.location.origin + "/video/" + id;
    navigator.clipboard.writeText(link);
    setTimeout(function(){
        el.innerHTML = oldHTML;
    }, 2000);
}

async function openEditVideo(id) {
    let videoResponse = await fetch(`/get-video?video_id=${id}`);
    if (!videoResponse.ok) {
        console.log("Error getting video " + id);
        return;
    }
    videoId = id;
    let video = await videoResponse.json();

    let room = document.getElementById("editRoom");
    room.value = video.room_id;
    await updateNodeOptions('editRoom', 'editFromNode', 'editToNode', 'editStrat');

    let fromNode = document.getElementById("editFromNode");
    fromNode.value = video.from_node_id;
    
    let toNode = document.getElementById("editToNode");
    toNode.value = video.to_node_id;

    await updateStratOptions('editRoom', 'editStrat', 'editFromNode', 'editToNode');
    let strat = document.getElementById("editStrat");
    strat.value = video.strat_id;

    let note = document.getElementById("editNote");
    note.value = video.note;

    let devNote = document.getElementById("editDevNote");
    devNote.value = video.dev_note;

    let cropSize = document.getElementById("edit-cropSize");
    cropSize.value = video.crop_size;

    let cropCenterX = document.getElementById("edit-cropCenterX");
    cropCenterX.value = video.crop_center_x;

    let cropCenterY = document.getElementById("edit-cropCenterY");
    cropCenterY.value = video.crop_center_y;

    let thumbnailT = document.getElementById("edit-thumbnailTime");
    thumbnailT.value = video.thumbnail_t;
    thumbnailT.removeAttribute("max");

    let highlightStartT = document.getElementById("edit-highlightStartTime");
    highlightStartT.value = video.highlight_start_t;
    highlightStartT.removeAttribute("max");

    let highlightEndT = document.getElementById("edit-highlightEndTime");
    highlightEndT.value = video.highlight_end_t;
    highlightEndT.min = video.highlight_start_t + 1;
    highlightEndT.removeAttribute("max");

    let priority = document.getElementById("edit-priority");
    priority.value = video.priority;

    let status = document.getElementById("editStatus");
    status.value = video.status;
    updateEditStatus();

    var form = document.getElementById("editForm");
    form.classList.remove('was-validated');

    numVideoParts = video.num_parts;
    document.getElementById("edit-show-preview").classList.remove("d-none");
    document.getElementById("edit-preview").classList.add("d-none");

    let permission = localStorage.getItem("permission");
    if (permission != "Editor") {
        document.getElementById("editStatusApproved").classList.add("d-none");
    } else {
        document.getElementById("editStatusApproved").classList.remove("d-none");
    }

    if (video.permanent) {
        document.getElementById("deleteVideoButton").classList.add("d-none");
    } else {
        document.getElementById("deleteVideoButton").classList.remove("d-none");
    }

    updateControls('edit-');
    controlsUpdated = false;
    let editModal = new bootstrap.Modal(document.getElementById("editModal"));
    editModal.show();
}

async function submitEditVideo() {
    if (submitting) {
        return;
    }
    submitting = true;
    let editModal = bootstrap.Modal.getInstance(document.getElementById("editModal"));

    var form = document.getElementById("editForm");
    if (!form.checkValidity()) {
        console.log("invalid edit form");
        form.classList.add('was-validated');
        submitting = false;
        return;
    }

    let req = {
        video_id: videoId,
        status: document.getElementById("editStatus").value,
        room_id: tryParseInt(document.getElementById("editRoom").value),
        from_node_id: tryParseInt(document.getElementById("editFromNode").value),
        to_node_id: tryParseInt(document.getElementById("editToNode").value),
        strat_id: tryParseInt(document.getElementById("editStrat").value),
        note: document.getElementById("editNote").value,
        dev_note: document.getElementById("editDevNote").value,
        crop_size: tryParseInt(document.getElementById("edit-cropSize").value),
        crop_center_x: tryParseInt(document.getElementById("edit-cropCenterX").value),
        crop_center_y: tryParseInt(document.getElementById("edit-cropCenterY").value),
        thumbnail_t: tryParseInt(document.getElementById("edit-thumbnailTime").value),
        highlight_start_t: tryParseInt(document.getElementById("edit-highlightStartTime").value),
        highlight_end_t: tryParseInt(document.getElementById("edit-highlightEndTime").value),
        priority: tryParseInt(document.getElementById("edit-priority").value),
        controls_updated: controlsUpdated,
    };
    var json = JSON.stringify(req);

    let username = localStorage.getItem("username");
    let token = localStorage.getItem("token");

    var result = await fetch("/edit-video", {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
            "Authorization": 'Basic ' + btoa(username + ":" + token),
        },
        body: json
    });
    submitting = false;
    editModal.hide();
    if (result.ok) {
        console.log("Successfully edited video");
        frameOffsets = null;
        document.getElementById("videoFile").value = null;
        updateFilter();
    } else {
        resultText = await result.text();
        console.log(`Failed to edit video: ${resultText}`);
    }
}

function updateEditStatus() {
    let status = document.getElementById("editStatus").value;
    let room = document.getElementById("editRoom");
    let fromNode = document.getElementById("editFromNode");
    let toNode = document.getElementById("editToNode");
    let strat = document.getElementById("editStrat");
    if (status == 'Complete' || status == 'Approved') {
        room.required = true;
        fromNode.required = true;
        toNode.required = true;
        strat.required = true;
    } else {
        room.required = false;
        fromNode.required = false;
        toNode.required = false;
        strat.required = false;
    }
    var form = document.getElementById("editForm");
    form.classList.remove('was-validated');
}

async function deleteVideo() {
    let editModal = bootstrap.Modal.getInstance(document.getElementById("editModal"));
    let username = localStorage.getItem("username");
    let token = localStorage.getItem("token");
    let response = await fetch(`/?video_id=${videoId}`, {
        "method": "DELETE",
        "headers": {
            "Authorization": 'Basic ' + btoa(username + ":" + token),        
        }
    });
    if (response.ok) {
        console.log(`Successfully deleted video: video_id=${videoId}`);
        editModal.hide();
        updateFilter();
    } else {
        console.log(`Error deleting video ${videoId}: ${await response.text()}`);
        editModal.show();        
    }
}

async function populateTech() {
    let response = await fetch("/tech");
    if (!response.ok) {
        console.log(`Error status ${response.status} loading tech: ${await response.text()}`);
        return;
    }
    let techJson = await response.json();
    let techByDifficulty = {};
    for (const difficulty of difficultyLevels) {
        techByDifficulty[difficulty] = [];
    }
    for (const tech of techJson) {
        techByDifficulty[tech["difficulty"]].push(tech);
    }
    for (const difficulty of difficultyLevels) {
        let difficultyNoSpace = difficulty.replace(/ /g, '');
        let countEl = document.getElementById(`difficultyCountTech${difficultyNoSpace}`);
        countEl.innerText = techByDifficulty[difficulty].length;

        let techTableBody = document.getElementById(`techTableBody${difficultyNoSpace}`);
        techTableBody.innerHTML = "";
        for (const tech of techByDifficulty[difficulty]) {
            let techId = tech["tech_id"];
            let tr = document.createElement('tr');
            let td = document.createElement('td');
            td.classList.add("p-2");
            let row = document.createElement('div');
            row.classList.add("row");
            row.classList.add("video-row");
    
            let imgCol = document.createElement('div');
            imgCol.classList.add("text-center");
            imgCol.classList.add("col-sm-4");
            imgCol.classList.add("col-lg-2");

            let pngEl = document.createElement('img');
            pngEl.classList.add("png");
            pngEl.loading = "lazy";
            pngEl.style = "width:128px;";
            pngEl.id = `techPng${techId}`;
            imgCol.appendChild(pngEl);

            let webpEl = document.createElement('img');
            webpEl.classList.add("webp");
            webpEl.loading = "lazy";
            webpEl.fetchPriority = "low";
            webpEl.style = "width:128px;";
            webpEl.id = `techWebp${techId}`;
            imgCol.appendChild(webpEl);

            let textCol = document.createElement('div');
            textCol.classList.add("col-sm-8");
            textCol.classList.add("col-lg-10");

            let techNameRow = document.createElement('div');
            techNameRow.classList.add("row");
            techNameRow.classList.add("m-2");
            let techNameP = document.createElement('p');
            let techNameB = document.createElement('b');
            techNameB.innerText = tech["name"];
            techNameP.appendChild(techNameB);
            techNameRow.appendChild(techNameP);
            textCol.appendChild(techNameRow);

            let videoIdRow = document.createElement('div');
            videoIdRow.classList.add("row");
            videoIdRow.classList.add("m-2");
            let videoIdLabelCol = document.createElement('div');
            videoIdLabelCol.classList.add("col-auto");
            videoIdLabelCol.classList.add("d-flex");
            videoIdLabelCol.classList.add("align-items-center");
            let videoIdLabel = document.createElement('label');
            videoIdLabel.classList.add("form-label");
            videoIdLabel.for = `techVideoId${techId}`;
            videoIdLabel.innerText = "Video Id";
            videoIdLabelCol.appendChild(videoIdLabel);
            videoIdRow.appendChild(videoIdLabelCol);

            let videoIdInputCol = document.createElement('div');
            videoIdInputCol.classList.add("col-auto");
            let videoIdInput = document.createElement('input');
            videoIdInput.classList.add("form-control");
            videoIdInput.id = `techVideoId${techId}`;
            videoIdInput.classList.add("video-id");
            videoIdInput.size = 6;
            videoIdInput.setAttribute("onchange", `updateTechVideo(${techId})`);
            videoIdInput.value = tech["video_id"];
            videoIdInputCol.appendChild(videoIdInput);
            videoIdRow.appendChild(videoIdInputCol);

            let difficultyLabelCol = document.createElement('div');
            difficultyLabelCol.classList.add("col-auto");
            difficultyLabelCol.classList.add("d-flex");
            difficultyLabelCol.classList.add("align-items-center");
            let difficultyLabel = document.createElement('label');
            difficultyLabel.classList.add("form-label");
            difficultyLabel.for = `techDifficulty${techId}`;
            difficultyLabel.innerText = "Difficulty";
            difficultyLabelCol.appendChild(difficultyLabel);
            videoIdRow.appendChild(difficultyLabelCol);

            let difficultySelectCol = document.createElement('div');
            difficultySelectCol.classList.add("col-auto");
            let difficultySelect = document.createElement('select');
            difficultySelect.classList.add("form-select");
            difficultySelect.id = `techDifficulty${techId}`;
            difficultySelect.setAttribute("onchange", `updateTechDifficulty(${techId})`);
            for (const d of difficultyLevels) {
                let difficultyOption = document.createElement('option');
                difficultyOption.value = d;
                difficultyOption.innerText = d;
                difficultySelect.appendChild(difficultyOption);
            }
            difficultySelect.value = difficulty;
            difficultySelectCol.appendChild(difficultySelect);
            videoIdRow.appendChild(difficultySelectCol);
            textCol.appendChild(videoIdRow);

            row.appendChild(imgCol);
            row.appendChild(textCol);
            td.appendChild(row);
            tr.appendChild(td);
            techTableBody.appendChild(tr);    

            updateTechVideo(techId);
        }
    }
    updatedTech = new Set();
}

async function populateNotables() {
    let response = await fetch("/notables");
    if (!response.ok) {
        console.log(`Error status ${response.status} loading notables: ${await response.text()}`);
        return;
    }
    let notableJson = await response.json();
    let notablesByDifficulty = {};
    for (const difficulty of difficultyLevels) {
        notablesByDifficulty[difficulty] = [];
    }
    for (const notable of notableJson) {
        notablesByDifficulty[notable["difficulty"]].push(notable);
    }
    for (const difficulty of difficultyLevels) {
        let difficultyNoSpace = difficulty.replace(/ /g, '');
        let countEl = document.getElementById(`difficultyCountNotables${difficultyNoSpace}`);
        countEl.innerText = notablesByDifficulty[difficulty].length;

        let notableTableBody = document.getElementById(`notableTableBody${difficultyNoSpace}`);
        notableTableBody.innerHTML = "";
        for (const notable of notablesByDifficulty[difficulty]) {
            let roomId = notable["room_id"];
            let notableId = notable["notable_id"];
            let comboId = `${roomId}n${notableId}`;
            let tr = document.createElement('tr');
            let td = document.createElement('td');
            td.classList.add("p-2");
            let row = document.createElement('div');
            row.classList.add("row");
            row.classList.add("video-row");
    
            let imgCol = document.createElement('div');
            imgCol.classList.add("text-center");
            imgCol.classList.add("col-sm-4");
            imgCol.classList.add("col-lg-2");

            let pngEl = document.createElement('img');
            pngEl.classList.add("png");
            pngEl.loading = "lazy";
            pngEl.style = "width:128px;";
            pngEl.id = `notablePng${comboId}`;
            imgCol.appendChild(pngEl);

            let webpEl = document.createElement('img');
            webpEl.classList.add("webp");
            webpEl.loading = "lazy";
            webpEl.fetchPriority = "low";
            webpEl.style = "width:128px;";
            webpEl.id = `notableWebp${comboId}`;
            imgCol.appendChild(webpEl);

            let textCol = document.createElement('div');
            textCol.classList.add("col-sm-8");
            textCol.classList.add("col-lg-10");

            let notableNameRow = document.createElement('div');
            notableNameRow.classList.add("row");
            notableNameRow.classList.add("m-2");
            let notableNameP = document.createElement('p');
            let notableNameB = document.createElement('b');
            notableNameB.innerText = `${notable["room_name"]}: ${notable["name"]}`;
            notableNameP.appendChild(notableNameB);
            notableNameRow.appendChild(notableNameP);
            textCol.appendChild(notableNameRow);

            let videoIdRow = document.createElement('div');
            videoIdRow.classList.add("row");
            videoIdRow.classList.add("m-2");
            let videoIdLabelCol = document.createElement('div');
            videoIdLabelCol.classList.add("col-auto");
            videoIdLabelCol.classList.add("d-flex");
            videoIdLabelCol.classList.add("align-items-center");
            let videoIdLabel = document.createElement('label');
            videoIdLabel.classList.add("form-label");
            videoIdLabel.for = `notableVideoId${comboId}`;
            videoIdLabel.innerText = "Video Id";
            videoIdLabelCol.appendChild(videoIdLabel);
            videoIdRow.appendChild(videoIdLabelCol);

            let videoIdInputCol = document.createElement('div');
            videoIdInputCol.classList.add("col-auto");
            let videoIdInput = document.createElement('input');
            videoIdInput.classList.add("form-control");
            videoIdInput.classList.add("video-id");
            videoIdInput.id = `notableVideoId${comboId}`;
            videoIdInput.size = 6;
            videoIdInput.setAttribute("onchange", `updateNotableVideo(${roomId}, ${notableId})`);
            videoIdInput.value = notable["video_id"];
            videoIdInputCol.appendChild(videoIdInput);
            videoIdRow.appendChild(videoIdInputCol);

            let difficultyLabelCol = document.createElement('div');
            difficultyLabelCol.classList.add("col-auto");
            difficultyLabelCol.classList.add("d-flex");
            difficultyLabelCol.classList.add("align-items-center");
            let difficultyLabel = document.createElement('label');
            difficultyLabel.classList.add("form-label");
            difficultyLabel.for = `notableDifficulty${comboId}`;
            difficultyLabel.innerText = "Difficulty";
            difficultyLabelCol.appendChild(difficultyLabel);
            videoIdRow.appendChild(difficultyLabelCol);

            let difficultySelectCol = document.createElement('div');
            difficultySelectCol.classList.add("col-auto");
            let difficultySelect = document.createElement('select');
            difficultySelect.classList.add("form-select");
            difficultySelect.id = `notableDifficulty${comboId}`;
            difficultySelect.setAttribute("onchange", `updateNotableDifficulty(${roomId}, ${notableId}); updateMissingVideoCount();`);
            for (const d of difficultyLevels) {
                let difficultyOption = document.createElement('option');
                difficultyOption.value = d;
                difficultyOption.innerText = d;
                difficultySelect.appendChild(difficultyOption);
            }
            difficultySelect.value = difficulty;
            difficultySelectCol.appendChild(difficultySelect);
            videoIdRow.appendChild(difficultySelectCol);
            textCol.appendChild(videoIdRow);

            row.appendChild(imgCol);
            row.appendChild(textCol);
            td.appendChild(row);
            tr.appendChild(td);
            notableTableBody.appendChild(tr);    

            updateNotableVideo(roomId, notableId);
        }
    }
    updateMissingVideoCount();
    updatedNotables = new Set();
}

async function openTech() {
    populateTech();
    populateNotables();
    updateMissingVideoCount();
}

function updateMissingVideoCount() {
    for (const difficulty of difficultyLevels) {
        let difficultyNoSpace = difficulty.replace(/ /g, '');
        let difficultyParent = `collapse${difficultyNoSpace}Tech`;
        var cnt = 0;
        for (const el of document.querySelectorAll(`#${difficultyParent} .video-id`)) {
            if (el.value === "") {
                cnt += 1;
            }
        }
        let missingCntEl = document.getElementById(`difficultyCountMissingVideos${difficultyNoSpace}`);
        if (cnt > 0) {
            missingCntEl.innerText = ` - ${cnt} missing videos`;
        } else {
            missingCntEl.innerText = "";
        }
    }
}

function updateTechVideo(techId) {
    updatedTech.add(techId);
    let videoIdEl = document.getElementById(`techVideoId${techId}`);
    let videoId = videoIdEl.value;
    let pngEl = document.getElementById(`techPng${techId}`);
    let webpEl = document.getElementById(`techWebp${techId}`);
    // let videoUrl = videoStorageClientUrl + "/mp4/" + videoId + ".mp4";
    if (videoId === "") {
        pngEl.classList.add("d-none");
        webpEl.classList.add("d-none");
    } else {
        pngEl.classList.remove("d-none");
        webpEl.classList.remove("d-none");
        pngEl.src = videoStorageClientUrl + "/png/" + videoId + ".png";        
        webpEl.src = videoStorageClientUrl + "/webp/" + videoId + ".webp";
    }
}

function updateTechDifficulty(techId) {
    updatedTech.add(techId);
}

function updateNotableVideo(roomId, notableId) {
    updatedNotables.add([roomId, notableId]);
    let comboId = `${roomId}n${notableId}`;
    let videoIdEl = document.getElementById(`notableVideoId${comboId}`);
    let videoId = videoIdEl.value;
    let pngEl = document.getElementById(`notablePng${comboId}`);
    let webpEl = document.getElementById(`notableWebp${comboId}`);
    // let videoUrl = videoStorageClientUrl + "/mp4/" + videoId + ".mp4";
    if (videoId === "") {
        pngEl.classList.add("d-none");
        webpEl.classList.add("d-none");
    } else {
        pngEl.classList.remove("d-none");
        webpEl.classList.remove("d-none");
        pngEl.src = videoStorageClientUrl + "/png/" + videoId + ".png";        
        webpEl.src = videoStorageClientUrl + "/webp/" + videoId + ".webp";
    }
    updateMissingVideoCount();
}

function updateNotableDifficulty(roomId, notableId) {
    updatedNotables.add([roomId, notableId]);
}

async function postTechUpdates() {
    let reqArray = [];
    for (const techId of updatedTech) {
        let difficulty = document.getElementById(`techDifficulty${techId}`).value;
        let videoId = parseInt(document.getElementById(`techVideoId${techId}`).value);
        reqArray.push({
            tech_id: techId,
            difficulty: difficulty,
            video_id: videoId, 
        });
    }
    let reqJson = JSON.stringify(reqArray);

    let username = localStorage.getItem("username");
    let token = localStorage.getItem("token");

    let response = await fetch("/tech", {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
            "Authorization": 'Basic ' + btoa(username + ":" + token),
        },
        body: reqJson
    });
    if (!response.ok) {
        throw new Error(`Error status ${response.status} updating tech: ${await response.text()}`);
    }
}

async function postNotableUpdates() {
    let reqArray = [];
    for (const ids of updatedNotables) {
        let roomId = ids[0];
        let notableId = ids[1];
        let comboId = `${roomId}n${notableId}`;
        let difficulty = document.getElementById(`notableDifficulty${comboId}`).value;
        let videoId = parseInt(document.getElementById(`notableVideoId${comboId}`).value);
        reqArray.push({
            room_id: roomId,
            notable_id: notableId,
            difficulty: difficulty,
            video_id: videoId, 
        });
    }
    let reqJson = JSON.stringify(reqArray);

    let username = localStorage.getItem("username");
    let token = localStorage.getItem("token");

    let response = await fetch("/notables", {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
            "Authorization": 'Basic ' + btoa(username + ":" + token),
        },
        body: reqJson
    });
    if (!response.ok) {
        throw new Error(`Error status ${response.status} updating notables: ${await response.text()}`);
    }
}

async function autoFillNotables() {
    let response = await fetch("/auto-pick-notable-videos");
    if (!response.ok) {
        console.log(`Error status ${response.status} loading notable videos: ${await response.text()}`);
        return;
    }
    let notableJson = await response.json();
    for (const data of notableJson) {
        let comboId = `${data.room_id}n${data.notable_id}`;
        let videoIdEl = document.getElementById(`notableVideoId${comboId}`);
        if (videoIdEl.value !== "") {
            continue;
        }
        videoIdEl.value = data.video_id;
        updateNotableVideo(data.room_id, data.notable_id);
        updatedNotables.add([data.room_id, data.notable_id]);
    }
    updateMissingVideoCount();
}

async function submitTech() {
    postTechUpdates();
    postNotableUpdates();
    let techModal = bootstrap.Modal.getInstance(document.getElementById("techModal"));
    techModal.hide();
}

function startVideo(url) {
    console.log("starting video ", url);
    video.pause();
    document.getElementById("videoSource").setAttribute("src", url);
    video.load();
    video.play();
}

document.getElementById("videoModal").addEventListener('hidden.bs.modal', (event) => {
    document.getElementById("video").pause();
});

document.getElementById("deleteModal").addEventListener('hidden.bs.modal', (event) => {
    let editModal = bootstrap.Modal.getInstance(document.getElementById("editModal"));
    editModal.show();
});

document.addEventListener('keydown', (ev) => {
    if (!document.getElementById("videoModal").classList.contains("show")) {
      return;
    }

    const video = document.getElementById('video')
    switch (ev.key) {
      case ",":
        if (!video.paused) {
          video.pause();
        }
        video.currentTime -= 1 / 60;
        break;
      case ".":
        if (!video.paused) {
          video.pause();
        }
        video.currentTime += 1 / 60;
        break;
      case " ":
        if (video.paused) {
          video.play();
        } else {
          video.pause();
        }
        break;
      case "f":
      case "F":
        video.requestFullscreen();
        break;
      case "ArrowRight":
        video.currentTime += 5;
        break;
      case "ArrowLeft":
        video.currentTime -= 5;
        break;
    }
});

window.addEventListener('scroll', () => {
    const bottomOfWindow = window.scrollY + window.innerHeight;
    const bottomOfDocument = document.documentElement.scrollHeight;
    const threshold = 1000; // 1000 pixels from the bottom

    // Load more videos into DOM if scrolled close enough to bottom:
    if (bottomOfWindow >= bottomOfDocument - threshold && videoLimit < videoList.length) {
        let newVideoLimit = Math.min(videoLimit + videoLimitIncrement, videoList.length);
        loadVideoBatch(videoLimit, newVideoLimit);
    }
});

updateLogin();
updateRoomOptions([document.getElementById("room"), document.getElementById("filterRoom"), document.getElementById("editRoom")]);
updateFile();
animateLoop();
updateUserList();
updateFilter();
