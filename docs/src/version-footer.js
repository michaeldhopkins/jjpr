document.addEventListener('DOMContentLoaded', function() {
    var nav = document.querySelector('.nav-wide-wrapper') || document.querySelector('.nav-wrapper');
    if (nav) {
        var footer = document.createElement('div');
        footer.className = 'version-footer';
        footer.textContent = 'jjpr v0.40.3';
        nav.parentNode.insertBefore(footer, nav.nextSibling);
    }
});
