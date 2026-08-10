(function () {
    "use strict";

    // Convert <date> content to local timezone, but with ISO 8601 format
    // because the default locale date formats suck.
    function processDateTime(time) {
        var date = new Date(time.dateTime);
        var local = "";
        local += date.getFullYear().toString();
        local += "-";
        local += ("00" + (date.getMonth() + 1)).substr(-2);
        local += "-";
        local += ("00" + date.getDate()).substr(-2);
        local += " ";
        local += ("00" + date.getHours()).substr(-2);
        local += ":";
        local += ("00" + date.getMinutes()).substr(-2);
        local += ":";
        local += ("00" + date.getSeconds()).substr(-2);
        time.textContent = local;
        // Let the user see the original date by hovering over it.
        time.title = time.dateTime;
    }

    // Enable image uploads. JavaScript is used to do client-side image resizing
    // and encoding, which not only simplifies the server-side code, but also
    // reduces the amount of data uploaded and protects the user's privacy
    // (since EXIF metadata etc is never transmitted).
    //
    // `field` is an <input type=hidden> that'll be used to submit the data URI
    // to the server.
    function processImageUploadField(field) {
        var fileInput = document.createElement('input');
        fileInput.type = 'file';
        fileInput.accept = 'image/*';

        var clearButton = document.createElement('button');
        clearButton.textContent = 'Clear';

        var preview = new Image;
        preview.title = preview.alt = 'Image preview';

        field.parentElement.insertBefore(fileInput, field.nextSibling);
        field.parentElement.insertBefore(clearButton, fileInput.nextSibling);
        field.parentElement.insertBefore(preview, clearButton.nextSibling);

        function clearFile() {
            field.value = '';
            preview.src = '';
            preview.style.display = 'none';
            fileInput.value = null;
            clearButton.disabled = true;
        }

        function setProcessedImage(dataUri) {
            field.value = dataUri;
            preview.src = dataUri;
            preview.style.display = 'inline-block';
            clearButton.disabled = false;
        }

        function processImage() {
            try {
                var source = this;

                var MAX_SIZE = 640; // in pixels, on either side
                var JPEG_QUALITY = 0.8; // 80%

                var largestSide = Math.max(source.width, source.height);
                var scale = Math.min(1, MAX_SIZE / largestSide);

                console.log("Source image is " + source.width + "×" + source.height);

                // Browsers often don't use mipmapping when downscaling images,
                // which means that scales of less than 0.5× can produce
                // noticeable aliasing artifacts. To avoid that, reduce in 0.5×
                // steps if necessary.
                while (scale < 0.5) {
                    var newSource = document.createElement('canvas');
                    newSource.width = source.width / 2;
                    newSource.height = source.height / 2;
                    console.log("0.5× reduction step: " + source.width + "×" + source.height + " → " + newSource.width + "×" + newSource.height);

                    var newSourceCtx = newSource.getContext('2d');
                    newSourceCtx.drawImage(source, 0, 0, newSource.width, newSource.height);

                    source = newSource;
                    scale *= 2;
                }

                var dest = document.createElement('canvas');
                dest.width = source.width * scale;
                dest.height = source.height * scale;

                var destCtx = dest.getContext('2d');
                destCtx.drawImage(source, 0, 0, dest.width, dest.height);

                var dataUri = dest.toDataURL('image/jpeg', JPEG_QUALITY);

                let sizeEstimate = dataUri.length * (6/8); // base64 compensation
                console.log('Processed image: ' + dest.width + '×' + dest.height + ', ' + Math.ceil(sizeEstimate / 1000) + ' KB');
                setProcessedImage(dataUri);
            } catch (e) {
                alert("Couldn't process image: " + e);
                clearFile();
            }
        }

        clearButton.onclick = function (e) {
            e.preventDefault();
            clearFile();
            return false;
        };
        fileInput.onchange = function () {
            try {
                var file = fileInput.files[0];
                if (!file) {
                    clearFile();
                    return;
                }

                var source = new Image;
                source.onload = processImage;
                source.onerror = function (e) {
                    alert("Couldn't load image");
                    clearFile();
                };
                source.src = URL.createObjectURL(file);
            } catch (e) {
                alert("Couldn't load image: " + e);
                clearFile();
            }
        };

        clearFile();
    }

    function processSearchableTable(table) {
        // Collect data about each row

        var rows = [];
        for (var i = 0; i < table.rows.length; i++) {
            var row = table.rows[i];
            if (row.parentElement.tagName === 'THEAD') {
                continue;
            }
            var cells = [];
            for (var j = 0; j < row.children.length; j++) {
                var td = row.children[j];
                if (td.tagName !== 'TD') {
                    continue;
                }
                cells.push(td.textContent.trim().toLowerCase());
            }
            rows.push({
                element: row,
                parentElement: row.parentElement,
                textContent: row.textContent.toLowerCase(),
                cells: cells,
            });
        }

        // Implement search

        var label = document.createElement('label');
        label.textContent = 'Search: ';
        var input = document.createElement('input');
        input.type = 'text';
        input.oninput = function () {
            var query = input.value.toLowerCase();
            for (var i = 0; i < rows.length; i++) {
                rows[i].element.style.display = (rows[i].textContent.includes(query) ? '' : 'none');
            }
        };
        label.appendChild(input);
        table.parentElement.insertBefore(label, table);

        // Implement sorting

        var sortBy = null;
        var sortAscending = null;
        var columns = [];

        for (var i = 0; i < table.tHead.rows[0].children.length; i++) {
            var th = table.tHead.rows[0].children[i];
            if (th.tagName !== 'TH') {
                continue;
            }
            columns.push({
                element: th,
                button: null,
            });
        }
        for (var i = 0; i < columns.length; i++) {
            var column = columns[i];
            // Hack to fix layout, see style.css
            column.element.innerHTML = '<span class=sortable-column-header><span class=sortable-column-button></span><span class=sortable-column-text>' + column.element.innerHTML + '</span></span>';
            var button = document.createElement('button');
            button.className = 'sortable-column-button';
            (function (button, i) {
                button.onclick = function () {
                    if (sortBy === i) {
                        if (sortAscending) {
                            sortAscending = false;
                        } else {
                            sortBy = null;
                        }
                    } else {
                        sortBy = i;
                        sortAscending = true;
                    }
                    updateButtons();
                    sort();
                };
            }(button, i));
            column.element.firstChild.firstChild.appendChild(button);
            column.button = button;
        }
        function updateButtons() {
            for (var i = 0; i < columns.length; i++) {
                var button = columns[i].button;
                if (sortBy === i) {
                    if (sortAscending) {
                        button.textContent = '▲';
                        button.title = 'Click to sort by this column (descending)';
                    } else {
                        button.textContent = '▼';
                        button.title = 'Click to stop sorting by this column';
                    }
                } else {
                    button.textContent = '-';
                    button.title = 'Click to sort by this column (ascending)';
                }
            }
        }
        updateButtons();
        function sort() {
            for (var i = 0; i < rows.length; i++) {
                rows[i].parentElement.removeChild(rows[i].element);
            }
            var rowsSorted;
            if (sortBy === null) {
                rowsSorted = rows;
            } else {
                rowsSorted = rows.slice().sort(function (a, b) {
                    a = a.cells[sortBy];
                    b = b.cells[sortBy];
                    var ordering;
                    if (a < b) {
                        ordering = -1;
                    } else if (a > b) {
                        ordering = 1;
                    } else {
                        ordering = 0;
                    }
                    return sortAscending ? ordering : -ordering;
                });
            }
            for (var i = 0; i < rowsSorted.length; i++) {
                rowsSorted[i].parentElement.appendChild(rowsSorted[i].element);
            }
        }
    }

    // UI for the reparenting moderation action: click on “Reparent” for the
    // report that needs moving, then click on “Reparent here” for destination
    // version it should be moved to.
    var currentReparentSource = null;
    function processReparentSource(reparentSource, reparentSources, reparentTargets) {
        var reparentSourceButton = reparentSource.querySelector('input[type=submit]');
        reparentSource.onsubmit = function () {
            if (currentReparentSource !== null) {
                currentReparentSource.querySelector('input[type=submit]').className = '';
            }
            if (currentReparentSource === reparentSource) {
                currentReparentSource = null;
                for (var i = 0; i < reparentTargets.length; i++) {
                    reparentTargets[i].querySelector('input[type=submit]').disabled = true;
                }
            } else {
                currentReparentSource = reparentSource;
                reparentSourceButton.className = 'current-reparent-source';
                for (var i = 0; i < reparentTargets.length; i++) {
                    reparentTargets[i].querySelector('input[type=submit]').disabled = false;
                }
            }
            return false; // Don't actually submit the form
        };
        reparentSourceButton.disabled = false;
    }
    function processReparentTarget(reparentTarget) {
        reparentTarget.onsubmit = function () {
            if (currentReparentSource !== null) {
                // The form for the reparent source (report) contains everything
                // needed for the reparenting operation, except for the version
                // ID, which is in the form for the reparent target (version),
                // as a <input type=hidden>. So when we can just move that field
                // to the other form and submit it directly (which bypasses the
                // onsubmit handler).
                if (confirm("Are you sure you want to ↗️ reparent this report?")) {
                    var hiddenField = reparentTarget.querySelector('input[type=hidden]');
                    currentReparentSource.appendChild(hiddenField);
                    currentReparentSource.submit();
                }
            }
            return false; // Don't actually submit the form
        };
    }

    document.addEventListener("DOMContentLoaded", function () {
        var times = document.getElementsByTagName("time");
        for (var i = 0; i < times.length; i++) {
            processDateTime(times[i]);
        }

        var imageFields = document.getElementsByClassName('image-upload');
        for (var i = 0; i < imageFields.length; i++) {
            processImageUploadField(imageFields[i]);
        }

        var searchableTables = document.getElementsByClassName('searchable-table');
        for (var i = 0; i < searchableTables.length; i++) {
            processSearchableTable(searchableTables[i]);
        }

        var reparentSources = document.getElementsByClassName('reparent-source');
        var reparentTargets = document.getElementsByClassName('reparent-target');
        for (var i = 0; i < reparentSources.length; i++) {
            processReparentSource(reparentSources[i], reparentSources, reparentTargets);
        }
        for (var i = 0; i < reparentTargets.length; i++) {
            processReparentTarget(reparentTargets[i]);
        }
    });
}());
