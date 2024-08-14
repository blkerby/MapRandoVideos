// TODO: Split this up to make it more manageable.
// It would probably be better to separate the HTML-serving API form the backend endpoints.
// Maybe use some kind of modern framework and make it less of a mess?
var frameOffsets = null;
var animationEnabled = false;
var animationFrameResolution = 3;
var animationFrame = 0;
var videoId = null;
var uploading = false;
var doneUploading = false;
var submitting = false;
var userMapping = null;

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
    if (frameOffsets == null) {
        return;
    }
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
    if (frameOffsets == null) {
        return;
    }
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
    doneUploading = false;  
    var videoFile = document.getElementById("videoFile");
    if (videoFile.files.length == 0) {
        return;
    }

    // TODO: handle multiple files
    var file = videoFile.files[0];

    await loadAVIMetadata(file);

    document.getElementById("thumbnail").classList.remove("d-none");
    document.getElementById("highlightStart").classList.remove("d-none");
    document.getElementById("highlightEnd").classList.remove("d-none");

    document.getElementById("thumbnailSize").value = 128;
    document.getElementById("thumbnailX").value = 128;
    document.getElementById("thumbnailY").value = 112;

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

    var start = performance.now();
    var compressedStream = file.stream().pipeThrough(new CompressionStream("gzip"));
    var compressedData = new Uint8Array(await new Response(compressedStream).arrayBuffer());
    var elapsedTime = performance.now() - start;
    console.log(`compression time elapsed (ms): ${elapsedTime}, compressed size: ${compressedData.length}`);

    let username = localStorage.getItem("username");
    let token = localStorage.getItem("token");

    var start = performance.now();
    var uploadResponse = await fetch("/upload-video", {
        method: "POST",
        headers: {
            "Content-Type": "video/avi",
            "Content-Encoding": "gzip",
            "Authorization": 'Basic ' + btoa(username + ":" + token),
        },
        body: compressedData,
    });
    var elapsedTime = performance.now() - start;
    console.log(`upload time elapsed (ms): ${elapsedTime}`);

    if (!uploadResponse.ok) {
        throw new Error(`Error uploading video: ${uploadResponse.status}`);
    }
    videoId = parseInt(await uploadResponse.text());
    doneUploading = true;
    console.log("finished uploading video: id=" + videoId);
}

async function updateRoomOptions(roomSelectList) {
    let overviewResponse = await fetch("/rooms-by-area");
    if (!overviewResponse.ok) {
        throw new Error(`Error fetching rooms.json: ${overviewResponse.status}`);
    }
    let overview = await overviewResponse.json();
    for (roomSelect of roomSelectList) {
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
}

function updateLogin() {
    let username = localStorage.getItem("username");
    let logoutButton = document.getElementById("logoutButton");
    let loginButton = document.getElementById("loginButton");
    let uploadButton = document.getElementById("uploadButton");
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
    while (!doneUploading) {
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
    for (userInfo of userList) {
        var opt = document.createElement('option');
        opt.value = userInfo.id;
        opt.innerText = userInfo.username;
        userSelect.appendChild(opt);

        userMapping[userInfo.id] = userInfo.username;
    }
}

async function updateFilter() {
    if (userMapping === null) {
        await updateUserList();
    }

    let userId = localStorage.getItem("userId");
    let permission = localStorage.getItem("permission");

    let room = document.getElementById("filterRoom").value;
    let fromNode = document.getElementById("filterFromNode").value;
    let toNode = document.getElementById("filterToNode").value;
    let strat = document.getElementById("filterStrat").value;
    let user = document.getElementById("filterUser").value;
    let statuses = [];
    
    for (s of document.getElementById("filterStatus").options) {
        if (s.selected) {
            statuses.push(s.value);
        }
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
    req.status_list = statuses;
    req.sort_by = "SubmittedTimestamp";
    
    // The backend supports pagination but we're not using it yet.
    // If we add a lot of videos, consider dynamically loading the table rows as the user scrolls down.
    req.limit = 10000;

    let params = new URLSearchParams(req).toString();
    let result = await fetch(`/list-videos?${params}`);
    if (!result.ok) {
        throw new Error(`HTTP ${result.status} fetching video list: ${await result.text()}`);
    }

    let videoList = await result.json();
    document.getElementById("videoCount").innerText = videoList.length;

    let videoTableBody = document.getElementById("videoTableBody");
    videoTableBody.innerHTML = "";
      
    let dateFormat = new Intl.DateTimeFormat(undefined, {
        year: 'numeric',
        month: 'short',
        day: 'numeric',
        hour12: 'false',
        hour: 'numeric',
        minute: '2-digit',
        timeZoneName: 'short',
    });
    for (const video of videoList) {
        let tr = document.createElement('tr');
        let td = document.createElement('td');
        td.classList.add("p-2");
        let row = document.createElement('div');
        row.classList.add("row");
        row.classList.add("video-row");

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
        webpEl.src = videoStorageClientUrl + "/webp/" + video.id + ".webp";
        webpEl.style = "width:128px;";
        imgA.appendChild(webpEl);

        let textCol = document.createElement('div');
        textCol.classList.add("col-sm-6");
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

        if (video.note !== "") {
            let pNote = document.createElement('p');
            pNote.classList.add("m-0");
            pNote.innerText = `Note: ${video.note}`;
            textCol.appendChild(pNote);    
        }

        let shareCol = document.createElement('div');
        shareCol.classList.add("col-sm-2");
        shareCol.classList.add("text-end");

        let shareButton = document.createElement('button');
        shareButton.classList.add("btn");
        shareButton.classList.add("btn-secondary");
        shareButton.classList.add("my-1")
        shareButton.setAttribute("onclick", `shareVideoLink(this, ${video.id})`);
        shareButton.innerHTML = '<i class="bi bi-clipboard"></i> Share';
        shareCol.appendChild(shareButton);

        if (permission == "Editor" || userId == video.updated_user_id) {
            let editButton = document.createElement('button');
            editButton.classList.add("btn");
            editButton.classList.add("btn-success");
            editButton.classList.add("my-1")
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
}

async function editShowPreview(video_id) {

}

function shareVideoLink(el, id) {
    let oldHTML = el.innerHTML;
    el.innerHTML = '<i class="bi bi-check2"></i> Copied';
    let link = window.location.origin + "?video_id=" + id;
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

    let cropSize = document.getElementById("editCropSize");
    cropSize.value = video.crop_size;

    let cropCenterX = document.getElementById("editCropCenterX");
    cropCenterX.value = video.crop_center_x;

    let cropCenterY = document.getElementById("editCropCenterY");
    cropCenterY.value = video.crop_center_y;

    let thumbnailT = document.getElementById("editThumbnailTime");
    thumbnailT.value = video.thumbnail_t;

    let highlightStartT = document.getElementById("editHighlightStartTime");
    highlightStartT.value = video.highlight_start_t;

    let highlightEndT = document.getElementById("editHighlightEndTime");
    highlightEndT.value = video.highlight_end_t;

    let status = document.getElementById("editStatus");
    status.value = video.status;
    updateEditStatus();

    var form = document.getElementById("editForm");
    form.classList.remove('was-validated');

    if (video.permanent) {
        document.getElementById("deleteVideoButton").classList.add("d-none");
    } else {
        document.getElementById("deleteVideoButton").classList.remove("d-none");
    }
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
        crop_size: tryParseInt(document.getElementById("editCropSize").value),
        crop_center_x: tryParseInt(document.getElementById("editCropCenterX").value),
        crop_center_y: tryParseInt(document.getElementById("editCropCenterY").value),
        thumbnail_t: tryParseInt(document.getElementById("editThumbnailTime").value),
        highlight_start_t: tryParseInt(document.getElementById("editHighlightStartTime").value),
        highlight_end_t: tryParseInt(document.getElementById("editHighlightEndTime").value),
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
    let response = await fetch(`/?video_id=${videoId}`, {
        "method": "DELETE"
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

updateLogin();
updateRoomOptions([document.getElementById("room"), document.getElementById("filterRoom"), document.getElementById("editRoom")]);
updateFile();
animateLoop();
updateUserList();
updateFilter();
